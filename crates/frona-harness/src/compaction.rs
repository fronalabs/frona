//! Pure compaction planning. Generation, budgets, triggers and durable commits
//! belong to the execution layer; this module never invokes a model or tool.

use rig_core::completion::{Message, message::UserContent, request::ToolDefinition};

use crate::inference::{
    compaction::{retained_suffix_start, summary_message},
    context::{compaction_group_boundaries, estimate_message_tokens, estimate_tokens},
};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CompactionError {
    #[error(
        "invalid compaction limits: target must be positive and no larger than available input"
    )]
    InvalidLimits,
    #[error(
        "invalid compaction configuration: require 0 < target < trigger <= 100 and output reservation smaller than the window"
    )]
    InvalidConfiguration,
    #[error("compaction returned tool calls or native structured output instead of summary text")]
    InvalidSummary,
    #[error("compaction did not reduce estimated input ({before} before, {after} after)")]
    NoReduction { before: usize, after: usize },
    #[error("invalid compaction history: {0}")]
    InvalidHistory(String),
    #[error(
        "context exceeds input allowance ({estimated} estimated tokens, {available} available); original history is unchanged"
    )]
    ContextOverflow { estimated: usize, available: usize },
    #[error("compaction summary must not be empty")]
    EmptySummary,
}

/// Standalone context policy. The window is an explicit estimate, not provider
/// metadata. None for output tokens uses the selected model setting or 8192.
#[derive(Debug, Clone, Copy)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub context_window: usize,
    pub max_output_tokens: Option<u64>,
    pub trigger_percent: u8,
    pub target_percent: u8,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_window: crate::inference::context::DEFAULT_CONTEXT_WINDOW,
            max_output_tokens: None,
            trigger_percent: 80,
            target_percent: 70,
        }
    }
}

/// Input allowance after reserving model output, and the desired retained size.
/// These are explicit planning inputs, not trigger/default policy or measured usage.
#[derive(Debug, Clone, Copy)]
pub struct CompactionLimits {
    pub available_input_tokens: usize,
    pub target_input_tokens: usize,
}

/// An exact request-view entry. Summary coverage is provenance, never evidence
/// that the original messages were sent verbatim to a model.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum HistoryViewEntry {
    Original(usize),
    Summary {
        message: Message,
        covered_original_positions: Vec<usize>,
    },
}

/// An owned description of a leading summary and retained original positions.
/// Retention can be noncontiguous to protect the active input during long loops.
/// It contains no model client, identity, database handle or application callback.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompactedView {
    pub(super) source_message_count: usize,
    pub(super) entries: Vec<HistoryViewEntry>,
}

impl CompactedView {
    pub fn source_message_count(&self) -> usize {
        self.source_message_count
    }
    pub fn entries(&self) -> &[HistoryViewEntry] {
        &self.entries
    }
}

/// A borrowed, immutable original history with a separately selected model view.
/// Restore a saved view against the same append-only original history; any newer
/// messages automatically follow its fixed leading summary and retained suffix.
#[derive(Debug, Clone)]
pub struct HistoryView<'a> {
    originals: &'a [Message],
    entries: Vec<HistoryViewEntry>,
}

impl<'a> HistoryView<'a> {
    pub fn new(originals: &'a [Message]) -> Self {
        Self {
            originals,
            entries: (0..originals.len())
                .map(HistoryViewEntry::Original)
                .collect(),
        }
    }

    pub fn from_compacted(
        originals: &'a [Message],
        compacted: &CompactedView,
    ) -> Result<Self, CompactionError> {
        Self::restore_with(originals, compacted, compaction_group_boundaries)
    }

    fn restore_with(
        originals: &'a [Message],
        compacted: &CompactedView,
        groups: fn(&[Message]) -> Result<Vec<usize>, String>,
    ) -> Result<Self, CompactionError> {
        if compacted.source_message_count > originals.len() {
            return Err(invalid(
                "compacted view refers to unavailable original messages",
            ));
        }
        let mut covered = vec![false; compacted.source_message_count];
        let mut seen = vec![false; compacted.source_message_count];
        let mut previous_original = None;
        for (index, entry) in compacted.entries.iter().enumerate() {
            match entry {
                HistoryViewEntry::Original(position)
                    if *position < seen.len()
                        && previous_original.is_none_or(|previous| previous < *position)
                        && !seen[*position] =>
                {
                    seen[*position] = true;
                    previous_original = Some(*position);
                }
                HistoryViewEntry::Summary {
                    message,
                    covered_original_positions,
                } if index == 0 => {
                    if covered_original_positions.is_empty()
                        || !covered_original_positions
                            .windows(2)
                            .all(|pair| pair[0] < pair[1])
                        || !matches!(message, Message::User { content } if matches!(content.as_slice(), [UserContent::Text(_)]))
                    {
                        return Err(invalid(
                            "summary must cover ordered unique positions and contain plain user text",
                        ));
                    }
                    for &position in covered_original_positions {
                        if position >= seen.len() {
                            return Err(invalid("summary position is outside its source"));
                        }
                        covered[position] = true;
                        seen[position] = true;
                    }
                }
                _ => {
                    return Err(invalid(
                        "view must be a leading summary and ordered original positions without overlap",
                    ));
                }
            }
        }
        if seen.iter().any(|seen| !seen) {
            return Err(invalid(
                "view omits original positions without summary coverage",
            ));
        }
        // Removal must preserve complete original protocol groups. The active
        // user input remains literal even inside a long single-input tool loop.
        let source = &originals[..compacted.source_message_count];
        let boundaries = groups(source).map_err(invalid)?;
        for range in boundaries.windows(2) {
            if covered[range[0]..range[1]]
                .iter()
                .any(|value| *value != covered[range[0]])
            {
                return Err(invalid("summary cuts through a tool-result group"));
            }
        }
        if source
            .iter()
            .rposition(is_user_input)
            .is_some_and(|position| covered[position])
        {
            return Err(invalid("summary cannot replace the active user input"));
        }
        let mut view = Self {
            originals,
            entries: compacted.entries.clone(),
        };
        groups(&view.messages()).map_err(invalid)?;
        // Later unresolved proposals can be displayed/restored without executing
        // them. Planning still requires the entire current history to be ready.
        view.entries.extend(
            (compacted.source_message_count..originals.len()).map(HistoryViewEntry::Original),
        );
        Ok(view)
    }

    /// Historical request evidence from the persistent runtime is not a JSONL
    /// lifecycle. Keep its native messages and legacy result identities intact.
    pub fn persisted(originals: &'a [Message]) -> Result<Self, CompactionError> {
        crate::inference::context::persisted_group_boundaries(originals).map_err(invalid)?;
        Ok(Self::new(originals))
    }

    /// Apply a whole-row prefix selected by the persistent compactor, using the
    /// same coverage/group/active-input validation as standalone compaction.
    /// The caller owns calibrated costs and asynchronous summary persistence.
    pub fn compact_persisted_prefix(
        originals: &'a [Message],
        prefix_end: usize,
        summary: Message,
    ) -> Result<Self, CompactionError> {
        if prefix_end == 0 || prefix_end > originals.len() {
            return Err(invalid("invalid persisted summary prefix"));
        }
        let compacted = CompactedView {
            source_message_count: originals.len(),
            entries: std::iter::once(HistoryViewEntry::Summary {
                message: summary,
                covered_original_positions: (0..prefix_end).collect(),
            })
            .chain((prefix_end..originals.len()).map(HistoryViewEntry::Original))
            .collect(),
        };
        Self::restore_with(
            originals,
            &compacted,
            crate::inference::context::persisted_group_boundaries,
        )
    }

    pub fn originals(&self) -> &'a [Message] {
        self.originals
    }
    pub fn entries(&self) -> &[HistoryViewEntry] {
        &self.entries
    }

    pub fn messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .map(|entry| match entry {
                HistoryViewEntry::Original(position) => self.originals[*position].clone(),
                HistoryViewEntry::Summary { message, .. } => message.clone(),
            })
            .collect()
    }

    /// Only these positions may mark original feedback as issued in a report.
    pub fn original_positions(&self) -> Vec<usize> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                HistoryViewEntry::Original(position) => Some(*position),
                HistoryViewEntry::Summary { .. } => None,
            })
            .collect()
    }

    pub fn estimated_tokens(&self, system: &str, tools: &[ToolDefinition]) -> usize {
        self.entries
            .iter()
            .fold(request_overhead(system, tools), |tokens, entry| {
                let message = match entry {
                    HistoryViewEntry::Original(position) => &self.originals[*position],
                    HistoryViewEntry::Summary { message, .. } => message,
                };
                tokens.saturating_add(estimate_message_tokens(message))
            })
    }

    /// Select complete older groups for summarization. The newest user content
    /// (including attachments or text accompanying results), its protocol group,
    /// and a recent complete suffix stay verbatim. No trigger policy is applied: callers decide when to plan.
    /// None means no older group can usefully be replaced; overflow is still an
    /// error, never permission to drop the active request.
    pub fn plan(
        &self,
        system: &str,
        tools: &[ToolDefinition],
        limits: CompactionLimits,
    ) -> Result<Option<CompactionPlan<'a>>, CompactionError> {
        if limits.target_input_tokens == 0
            || limits.target_input_tokens > limits.available_input_tokens
        {
            return Err(CompactionError::InvalidLimits);
        }
        let messages = self.messages();
        compaction_group_boundaries(self.originals).map_err(invalid)?;
        let summary_end = usize::from(matches!(
            self.entries.first(),
            Some(HistoryViewEntry::Summary { .. })
        ));
        let tail = &messages[summary_end..];
        let boundaries = compaction_group_boundaries(tail).map_err(invalid)?;
        let fixed = request_overhead(system, tools);
        let total = fixed.saturating_add(message_tokens(&messages));
        if tail.is_empty() {
            ensure_fits(total, limits.available_input_tokens)?;
            return Ok(None);
        }
        let protected_message = tail.iter().rposition(is_user_input).unwrap_or(0);
        let protected = boundaries
            .windows(2)
            .position(|range| range[0] <= protected_message && protected_message < range[1])
            .expect("validated groups cover every message");
        let mut costs: Vec<_> = boundaries
            .windows(2)
            .map(|range| message_tokens(&tail[range[0]..range[1]]))
            .collect();
        let latest = costs.len() - 1;
        // Preserve the active input group separately, so a single input followed
        // by many completed tool/submit rounds can still be compacted.
        let protected_tokens = costs[protected];
        costs[protected] = 0;
        let required = fixed
            .saturating_add(protected_tokens)
            .saturating_add(costs[latest]);
        ensure_fits(required, limits.available_input_tokens)?;
        let previous_summary = message_tokens(&messages[..summary_end]);
        let start = retained_suffix_start(
            &costs,
            latest,
            fixed.saturating_add(protected_tokens),
            previous_summary,
            limits.target_input_tokens,
        );
        let keep_from = summary_end + boundaries[start];
        let protected_range =
            summary_end + boundaries[protected]..summary_end + boundaries[protected + 1];
        let removed: Vec<_> = (summary_end..keep_from)
            .filter(|index| !protected_range.contains(index))
            .collect();
        if removed.is_empty() {
            ensure_fits(total, limits.available_input_tokens)?;
            return Ok(None);
        }
        let mut covered: std::collections::BTreeSet<_> =
            self.summary_coverage().iter().copied().collect();
        covered.extend(removed.iter().map(|&index| match self.entries[index] {
            HistoryViewEntry::Original(position) => position,
            HistoryViewEntry::Summary { .. } => unreachable!("summary is leading"),
        }));
        let retained = self
            .entries
            .iter()
            .enumerate()
            .skip(summary_end)
            .filter(|(index, _)| *index >= keep_from || protected_range.contains(index))
            .map(|(_, entry)| entry.clone())
            .collect();
        let summary_input_original_positions = self.entries[..keep_from]
            .iter()
            .filter_map(|entry| match entry {
                HistoryViewEntry::Original(position) => Some(*position),
                HistoryViewEntry::Summary { .. } => None,
            })
            .collect();
        Ok(Some(CompactionPlan {
            originals: self.originals,
            summary_input: messages[..keep_from].to_vec(),
            retained,
            covered: covered.into_iter().collect(),
            summary_input_original_positions,
            fixed_tokens: fixed,
            limits,
        }))
    }

    fn summary_coverage(&self) -> &[usize] {
        match self.entries.first() {
            Some(HistoryViewEntry::Summary {
                covered_original_positions,
                ..
            }) => covered_original_positions,
            _ => &[],
        }
    }
}

/// A prepared selection tied to the borrowed original history. Supplying summary
/// text builds a separate view; it never edits originals or accepts loop results.
#[derive(Debug)]
pub struct CompactionPlan<'a> {
    originals: &'a [Message],
    summary_input: Vec<Message>,
    retained: Vec<HistoryViewEntry>,
    covered: Vec<usize>,
    summary_input_original_positions: Vec<usize>,
    fixed_tokens: usize,
    limits: CompactionLimits,
}

impl CompactionPlan<'_> {
    /// Exact older model-facing prefix, including the previous summary when any.
    /// The executor adds its summarization instruction and checks that request's
    /// limits before invoking a model; these messages authorize no tool execution.
    pub fn summary_input(&self) -> &[Message] {
        &self.summary_input
    }
    /// Positions replaced by the summary. A retained active input can also be
    /// present in summary_input as context without belonging to this coverage.
    pub fn covered_original_positions(&self) -> &[usize] {
        &self.covered
    }

    /// Exact original messages supplied verbatim to the summarization request.
    /// Previous summary coverage is excluded; use this mapping for live reports.
    pub fn summary_input_original_positions(&self) -> &[usize] {
        &self.summary_input_original_positions
    }

    /// Context is already rendered/frozen by the caller. It is included inside
    /// the same leading summary message, matching the persistent runtime layout.
    /// The adapter retains its snapshot signature separately.
    pub fn finish(
        &self,
        summary: &str,
        context: Option<&str>,
    ) -> Result<CompactedView, CompactionError> {
        if summary.trim().is_empty() {
            return Err(CompactionError::EmptySummary);
        }
        let mut entries = vec![HistoryViewEntry::Summary {
            message: summary_message(summary, context),
            covered_original_positions: self.covered.clone(),
        }];
        entries.extend(self.retained.clone());
        let compacted = CompactedView {
            source_message_count: self.originals.len(),
            entries,
        };
        let view = HistoryView::from_compacted(self.originals, &compacted)?;
        ensure_fits(
            self.fixed_tokens
                .saturating_add(message_tokens(&view.messages())),
            self.limits.available_input_tokens,
        )?;
        Ok(compacted)
    }
}

fn is_user_input(message: &Message) -> bool {
    matches!(message, Message::User { content } if content.iter().any(|item| !matches!(item, UserContent::ToolResult(_))))
}
fn message_tokens(messages: &[Message]) -> usize {
    messages.iter().fold(0usize, |n, message| {
        n.saturating_add(estimate_message_tokens(message))
    })
}
fn request_overhead(system: &str, tools: &[ToolDefinition]) -> usize {
    tools.iter().fold(estimate_tokens(system), |n, tool| {
        n.saturating_add(estimate_tokens(
            &serde_json::to_string(tool).expect("tool definitions are serializable"),
        ))
    })
}
fn ensure_fits(estimated: usize, available: usize) -> Result<(), CompactionError> {
    if estimated > available {
        Err(CompactionError::ContextOverflow {
            estimated,
            available,
        })
    } else {
        Ok(())
    }
}
fn invalid(reason: impl Into<String>) -> CompactionError {
    CompactionError::InvalidHistory(reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActionResolution, Conversation, ModelOutput, ToolOutput, UsageReport};
    use rig_core::completion::{
        AssistantContent,
        message::{ToolCall, ToolFunction},
    };

    #[test]
    fn restored_descriptors_cannot_omit_overlap_reorder_or_split_original_groups() {
        let mut conversation = Conversation::new("active instruction");
        let actions = conversation
            .record_response(
                ModelOutput::new(
                    vec![AssistantContent::ToolCall(ToolCall::from_wire(
                        "call",
                        ToolFunction::new("lookup".into(), serde_json::json!({})),
                    ))],
                    UsageReport::Unavailable,
                ),
                vec![],
            )
            .unwrap();
        conversation
            .resolve(
                &actions.actions[0],
                ActionResolution::Result(ToolOutput::text("result")),
            )
            .unwrap();
        conversation
            .record_response(
                ModelOutput::new(
                    vec![AssistantContent::text("later response")],
                    UsageReport::Unavailable,
                ),
                vec![],
            )
            .unwrap();
        let originals = conversation.messages();
        let descriptor = |covered: Vec<usize>, retained: Vec<usize>| CompactedView {
            source_message_count: 4,
            entries: std::iter::once(HistoryViewEntry::Summary {
                message: summary_message("facts", None),
                covered_original_positions: covered,
            })
            .chain(retained.into_iter().map(HistoryViewEntry::Original))
            .collect(),
        };
        assert!(
            HistoryView::from_compacted(originals, &descriptor(vec![1, 2], vec![0, 3])).is_ok()
        );
        for bad in [
            descriptor(vec![1], vec![0, 2, 3]),    // split call from result
            descriptor(vec![1, 2], vec![0, 2, 3]), // overlap
            descriptor(vec![1, 2], vec![0]),       // omitted message
            descriptor(vec![1, 2], vec![3, 0]),    // reorder
            descriptor(vec![0, 1, 2], vec![3]),    // hidden active input
            descriptor(vec![1, 2, 4], vec![0, 3]), // out-of-range provenance
        ] {
            assert!(matches!(
                HistoryView::from_compacted(originals, &bad),
                Err(CompactionError::InvalidHistory(_))
            ));
        }
    }
}
