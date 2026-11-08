//! Replay applies only protocol transitions. No runtime/client/callback is accepted.
use super::{Conversation, PendingAction};
use crate::{
    ActionKind, ActionPosition, ActionResolution, ActionStatus, ConversationError,
    ConversationStatus, HistoryCheckpoint, HistoryCompletion, HistoryDecodeError,
    HistoryDecodeErrorKind, HistoryEvent, HistoryRecord, HistoryView, HistoryViewEntry,
    history::History,
};
use rig_core::completion::{Message, message::UserContent};

/// Pure reconstructed evidence. Completion is historical protocol evidence and
/// never an accepted application value. No report, budget, or writer is restored.
#[derive(Debug)]
pub struct HistoryReplay {
    pub conversation: Conversation,
    /// Latest opaque checkpoint and its own sequence number. No compatibility fallback.
    pub checkpoint: Option<(u64, HistoryCheckpoint)>,
    pub checkpoint_stale: bool,
}

/// Deterministically reconstruct a complete typed prefix. Stopped diagnostics can
/// be followed by explicit transitions; finished histories allow checkpoints only.
/// Pending submissions always require fresh typed validation before live acceptance.
pub fn replay_history(records: &[HistoryRecord]) -> Result<HistoryReplay, HistoryDecodeError> {
    let state_error = |seq, message: String| {
        HistoryDecodeError::new(HistoryDecodeErrorKind::InvalidState, seq, message)
    };
    let Some(first) = records.first() else {
        return Err(HistoryDecodeError::new(
            HistoryDecodeErrorKind::MissingOpened,
            0,
            "no complete Opened record",
        ));
    };
    let mut conversation = Conversation::empty_with_id(first.conversation);
    for (index, record) in records.iter().enumerate() {
        let seq = u64::try_from(index).map_err(|e| state_error(u64::MAX, e.to_string()))?;
        if record.version != 1 {
            return Err(HistoryDecodeError::new(
                HistoryDecodeErrorKind::UnsupportedVersion,
                seq,
                "unsupported version",
            ));
        }
        if record.seq != seq || record.conversation != first.conversation {
            return Err(state_error(seq, "sequence or conversation mismatch".into()));
        }
        if index == 0 {
            let HistoryEvent::Opened {
                initial,
                instructions,
                mode,
            } = &record.event
            else {
                return Err(HistoryDecodeError::new(
                    HistoryDecodeErrorKind::MissingOpened,
                    seq,
                    "first record must be Opened",
                ));
            };
            if initial.as_ref().is_some_and(|content| {
                content
                    .iter()
                    .any(|c| matches!(c, UserContent::ToolResult(_)))
            }) {
                return Err(state_error(
                    seq,
                    "initial input contains unpaired results".into(),
                ));
            }
            if let Some(content) = initial {
                conversation.messages.push(Message::User {
                    content: content.clone(),
                });
            }
            conversation.instructions = instructions.clone();
            conversation.mode = mode.clone();
            conversation.history = History::from_records(vec![record.clone()]);
            continue;
        }
        if let HistoryEvent::Checkpoint { checkpoint: saved } = &record.event {
            if saved.through_seq != seq - 1 {
                return Err(state_error(
                    seq,
                    "checkpoint must refer to exactly the preceding prefix".into(),
                ));
            }
            continue;
        }
        if conversation.status == ConversationStatus::Finished {
            return Err(state_error(
                seq,
                "only checkpoints may follow completion".into(),
            ));
        }
        // Stopped is a diagnostic boundary, not acceptance or permanent sealing.
        conversation.status = ConversationStatus::Active;
        let applied = apply(&mut conversation, &record.event);
        applied.map_err(|e| state_error(seq, e))?;
    }
    // Keep exact source evidence, including completion kind, raw outputs, and
    // checkpoints. Shared memory transitions above cannot invoke external code.
    conversation.history = History::from_records(records.to_vec());
    let checkpoint = conversation.checkpoint();
    let checkpoint_stale = checkpoint.is_some_and(|saved| saved.stale);
    let checkpoint = checkpoint.map(|saved| (saved.seq, saved.checkpoint.clone()));
    Ok(HistoryReplay {
        conversation,
        checkpoint,
        checkpoint_stale,
    })
}

fn apply(conversation: &mut Conversation, event: &HistoryEvent) -> Result<(), String> {
    let protocol = |e: ConversationError| e.to_string();
    let handle = |position: &ActionPosition| PendingAction {
        conversation: position.conversation,
        response: position.response,
        slot: position.action,
    };
    match event {
        HistoryEvent::Opened { .. } => return Err("duplicate Opened".into()),
        HistoryEvent::User { message, content } => {
            if *message != conversation.messages.len() {
                return Err("invalid user position".into());
            }
            conversation.push_user(content.clone()).map_err(protocol)?;
        }
        HistoryEvent::Response {
            response,
            message,
            origin,
            offered_tools,
            output,
        } => {
            if *message != conversation.messages.len() || *response != conversation.responses.len()
            {
                return Err("invalid response position".into());
            }
            conversation
                .record_response_from(output.clone(), offered_tools.clone(), origin.clone())
                .map_err(protocol)?;
        }
        HistoryEvent::ToolStarted { action, ordinal } => {
            let handle = handle(action);
            let state = conversation.validate(&handle).map_err(protocol)?;
            if state.kind != ActionKind::Tool
                || state.invocation_ordinal.checked_add(1) != Some(*ordinal)
            {
                return Err("invalid invocation kind or ordinal".into());
            }
            let state = &mut conversation.responses[handle.response].actions[handle.slot];
            state.status = ActionStatus::Started;
            state.invocation_ordinal = *ordinal;
        }
        HistoryEvent::ActionResolved { action, resolution } => {
            conversation
                .resolve(&handle(action), resolution.clone())
                .map_err(protocol)?;
        }
        HistoryEvent::SubmissionCompleted {
            action,
            acknowledgement,
            ..
        } => {
            let handle = handle(action);
            let state = conversation.validate(&handle).map_err(protocol)?;
            if state.kind != ActionKind::Submission
                || state.status != ActionStatus::Proposed
                || conversation.pending != 1
            {
                return Err("completion requires the last pending submission".into());
            }
            // Historical evidence does not validate T, invoke an inspector, or
            // materialize A. The live validation bit stays false.
            conversation
                .apply_resolution(&handle, ActionResolution::Result(acknowledgement.clone()));
            conversation.status = ConversationStatus::Finished;
        }
        HistoryEvent::Compacted { view, .. } => {
            conversation.require_ready().map_err(protocol)?;
            if view.source_message_count() != conversation.messages.len() {
                return Err("compaction source prefix mismatch".into());
            }
            if !matches!(
                view.entries().first(),
                Some(HistoryViewEntry::Summary { .. })
            ) {
                return Err("compaction requires a leading summary".into());
            }
            HistoryView::from_compacted(&conversation.messages, view).map_err(|e| e.to_string())?;
            conversation.compacted = Some(view.clone());
        }
        HistoryEvent::Finished { kind } => {
            if let HistoryCompletion::Text { response } = kind
                && (response.checked_add(1) != Some(conversation.responses.len())
                    || conversation.final_text().is_none())
            {
                return Err(
                    "text completion requires the latest tool-free nonempty text response".into(),
                );
            }
            conversation.finish_with_kind(*kind).map_err(protocol)?;
        }
        HistoryEvent::Stopped { .. } => conversation.mark_stopped(),
        HistoryEvent::Checkpoint { .. } => unreachable!("handled before terminal check"),
    }
    Ok(())
}
