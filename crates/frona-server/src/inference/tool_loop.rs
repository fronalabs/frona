use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};
use tokio_util::sync::CancellationToken;

use crate::chat::broadcast::EventSender;
use crate::chat::message::models::{MessageResponse, Reasoning};

use crate::core::error::AppError;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::{InferenceContext, ToolDefinition, active_chat};

use crate::inference::streaming::{ModelGroupStreaming, StreamResult};
use frona_harness::inference::ModelGroup;

pub struct InferenceEvent {
    pub kind: InferenceEventKind,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum InferenceEventKind {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        provider_call_id: String,
        name: String,
        arguments: serde_json::Value,
        description: Option<String>,
    },
    ToolResult {
        id: String,
        name: String,
        result: String,
        success: bool,
    },
    EntityUpdated {
        table: String,
        record_id: String,
        fields: serde_json::Value,
    },
    Retry {
        retry_after_ms: u64,
        reason: &'static str,
    },

    /// Channel adapters use this to begin a "thinking/typing" affordance.
    Start,
    /// `message` is the persisted final state.
    Done {
        message: MessageResponse,
    },
    Cancelled {
        reason: String,
    },
    Failed {
        error: crate::chat::message::error::MessageError,
        message_id: String,
    },
    /// Loop is parked, waiting for something external (the human, a sibling
    /// task, a webhook) to resume it. The `reason` carries WHY; the message
    /// is the executing-status message at the point of the pause. Every
    /// pause cause fires this - adapters / FE that just want "loop stopped
    /// streaming" can match on `Paused { .. }` without inspecting reason.
    Paused {
        reason: PauseReason,
        message: MessageResponse,
    },
    /// Human just resolved a HITL - the loop is about to resume. The message
    /// reflects the post-resolution state (resolved tool_call.result set).
    Resume {
        message: MessageResponse,
    },
}

/// Why the inference loop paused. Each variant gets its own dispatcher
/// branch on the channel and FE sides - adding a new pause cause is one
/// new variant + one new branch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum PauseReason {
    /// Loop paused on pending HITL prompts. The pending tool_calls are on
    /// `message.tool_calls` (entries where `hitl.status == Pending`).
    Hitl,
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub provider_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub result: String,
    pub success: bool,
    pub duration_ms: u64,
    pub hitl: Option<crate::inference::hitl::Hitl>,
    pub task_event: Option<crate::inference::tool_call::TaskEvent>,
    pub system_prompt: Option<String>,
}

/// Compatibility name for the persistent runtime's explicit execution outcome.
pub type ToolLoopOutcome = super::InferenceResponse;

pub fn extract_reasoning(contents: &[AssistantContent]) -> Option<Reasoning> {
    contents.iter().find_map(|c| {
        if let AssistantContent::Reasoning(r) = c {
            Some(Reasoning {
                id: c
                    .native_item()
                    .and_then(|item| item.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                content: r.text.clone(),
                signature: c
                    .native_item()
                    .and_then(|item| item.get("signature"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                raw: serde_json::to_value(r).ok(),
            })
        } else {
            None
        }
    })
}

pub(crate) fn to_rig_tool_definitions(
    defs: &[ToolDefinition],
    exclude_mcp: bool,
) -> Vec<RigToolDefinition> {
    let mut definitions: Vec<_> = defs
        .iter()
        .filter(|d| !exclude_mcp || !d.id.starts_with("mcp__"))
        .map(|d| {
            let description = if d.provider_id.is_empty() {
                d.description.clone()
            } else {
                format!("{}\n\nTool group: {}", d.description, d.provider_id)
            };
            let mut parameters = d.parameters.clone();
            parameters.sort_all_objects();
            RigToolDefinition {
                name: rig_core::message::ToolName::new(d.id.clone()).expect("registered tool name"),
                description,
                parameters,
                annotations: Default::default(),
            }
        })
        .collect();
    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    definitions
}

fn extend_unique_attachments(
    all_attachments: &mut Vec<crate::storage::Attachment>,
    attachments: &[crate::storage::Attachment],
) {
    for attachment in attachments {
        let already_attached = all_attachments
            .iter()
            .any(|existing| existing.owner == attachment.owner && existing.path == attachment.path);
        if !already_attached {
            all_attachments.push(attachment.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tool_loop(
    model_group: &ModelGroup,
    system_prompt: &str,
    chat_history: Vec<RigMessage>,
    tool_registry: &AgentToolRegistry,
    event_tx: EventSender,
    cancel_token: CancellationToken,
    ctx: &InferenceContext,
    usage_service: &crate::inference::usage::UsageService,
    chat_service: &crate::chat::service::ChatService,
    message_id: &str,
    input_token_estimate: Option<frona_harness::inference::context::InputTokenEstimate>,
) -> Result<ToolLoopOutcome, AppError> {
    let controls = super::RuntimeControls::default();
    let report = controls.report();
    report
        .record_run(async {
            let result = run_tool_loop_controlled(
                model_group,
                system_prompt,
                chat_history,
                tool_registry,
                event_tx,
                ctx,
                usage_service,
                chat_service,
                message_id,
                input_token_estimate,
                controls.execution(&cancel_token),
            )
            .await;
            let state = crate::inference::controls::outcome(result.as_ref());
            (result, state)
        })
        .await
}

/// Persistent host orchestration over shared conversation, admission and action
/// dispatch. The historical native prefix is frozen; only the live suffix owns
/// SDK action identities. No historical row is synthesized into an SDK event.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_loop_controlled(
    model_group: &ModelGroup,
    system_prompt: &str,
    chat_history: Vec<RigMessage>,
    tool_registry: &AgentToolRegistry,
    event_tx: EventSender,
    ctx: &InferenceContext,
    usage_service: &crate::inference::usage::UsageService,
    chat_service: &crate::chat::service::ChatService,
    message_id: &str,
    mut input_token_estimate: Option<frona_harness::inference::context::InputTokenEstimate>,
    execution: frona_harness::ExecutionControl<'_>,
) -> Result<ToolLoopOutcome, AppError> {
    use crate::tool::persistent_dispatch::{
        PersistentToolDispatcher, PersistentToolOutcome, project_messages,
    };
    use frona_harness::{Conversation, ExecutionStop};
    let rig_tools =
        to_rig_tool_definitions(tool_registry.definitions(), tool_registry.mcp_bridge_mode());
    let mut conversation = Conversation::from_execution(execution);
    let mut all_attachments = Vec::new();
    let mut last_reasoning = None;
    let mut last_text = String::new();
    // A no-tool text answer still needs one call even with a zero exploration
    // cap; hallucinated proposals can consume bounded repair turns.
    let turns = if tool_registry.is_empty() {
        model_group.inference.max_tool_turns.max(1)
    } else {
        model_group.inference.max_tool_turns
    };
    let first_turn = if turns == 0 {
        0
    } else {
        chat_service.next_provider_turn(message_id).await?
    };
    for turn in 0..turns {
        let persisted_turn = first_turn
            .checked_add(turn as u32)
            .ok_or_else(|| AppError::Internal("Provider turn index exhausted".into()))?;
        if ctx.shutdown_token.is_cancelled() {
            return shutdown(
                &mut conversation,
                last_text,
                all_attachments,
                last_reasoning,
            );
        }
        let projected = project_messages(&conversation.request_messages().map_err(protocol_error)?);
        let input = conversation
            .projected_input(&projected)
            .map_err(protocol_error)?;
        let history = chat_history.iter().cloned().chain(projected).collect();
        let kind = if tool_registry.is_empty() {
            super::usage::InferenceKind::Text {
                agent_id: ctx.agent.id.clone(),
                chat_id: active_chat(ctx)?.id.clone(),
                message_id: message_id.into(),
            }
        } else {
            super::usage::InferenceKind::ToolTurn {
                agent_id: ctx.agent.id.clone(),
                chat_id: active_chat(ctx)?.id.clone(),
                message_id: message_id.into(),
                turn_index: turn as u32,
            }
        };
        let usage_ctx =
            super::usage::UsageContext::new(kind, ctx.user.id.clone(), model_group.name.clone());
        let mut turn_text = String::new();
        let response = model_group
            .stream_inference(
                frona_harness::inference::ModelRequest {
                    input_token_estimate: input_token_estimate.take(),
                    system_prompt,
                    history,
                    tools: rig_tools.clone(),
                    observations: frona_harness::inference::observation::Observations::new(
                        &usage_service.scoped(&usage_ctx),
                    ),
                    overrides: Default::default(),
                },
                &event_tx,
                input.execution(execution).map_err(protocol_error)?,
                &mut turn_text,
            )
            .await;
        let output = match response {
            Ok(StreamResult::Contents(output)) => *output,
            Ok(StreamResult::Cancelled) => {
                return stopped(
                    &mut conversation,
                    ExecutionStop::Cancelled("runtime cancelled".into()),
                    turn_text,
                    all_attachments,
                    last_reasoning,
                );
            }
            Err(error) => match crate::tool::sdk_adapter::legacy_error(error) {
                frona_harness::ToolError::Stop(stop) => {
                    return stopped(
                        &mut conversation,
                        stop,
                        turn_text,
                        all_attachments,
                        last_reasoning,
                    );
                }
                frona_harness::ToolError::Failed(error) => {
                    conversation.stop().map_err(protocol_error)?;
                    return Err(*error
                        .downcast::<AppError>()
                        .unwrap_or_else(|error| Box::new(AppError::Internal(error.to_string()))));
                }
                frona_harness::ToolError::Feedback(_) => {
                    unreachable!("legacy errors are not rich feedback")
                }
            },
        };
        let assistant = rig_core::message::AssistantMessage::new(output.content.clone())
            .with_origin(output.origin.clone())
            .with_stop(output.stop.clone());
        chat_service
            .record_provider_turn(message_id, persisted_turn, &assistant)
            .await?;
        last_reasoning = extract_reasoning(&output.content);
        last_text = turn_text.clone();
        let actions = conversation
            .record_response(output, rig_tools.clone())
            .map_err(protocol_error)?;
        if ctx.shutdown_token.is_cancelled() {
            return shutdown(
                &mut conversation,
                turn_text,
                all_attachments,
                last_reasoning,
            );
        }
        if let Err(error) = execution.check_cancellation() {
            return stopped(
                &mut conversation,
                error.into_execution_stop().expect("cancellation stop"),
                turn_text,
                all_attachments,
                last_reasoning,
            );
        }
        if actions.actions.is_empty() {
            conversation.finish().map_err(protocol_error)?;
            return Ok(ToolLoopOutcome::Completed {
                text: turn_text,
                attachments: all_attachments,
                lifecycle_event: None,
                reasoning: last_reasoning,
            });
        }
        let mut dispatcher = PersistentToolDispatcher::new(
            chat_service,
            tool_registry,
            ctx,
            message_id,
            persisted_turn,
            (!turn_text.is_empty()).then_some(turn_text.clone()),
            last_reasoning.clone(),
        )
        .await?
        .with_events(event_tx.clone());
        let mut pending = Vec::new();
        let mut task_event = None;
        let mut prompts = Vec::new();
        let results = match dispatcher
            .execute_batch(
                &mut conversation,
                &actions.actions,
                execution,
                model_group.inference.max_concurrent_tool_calls,
            )
            .await
        {
            Ok(result) => result,
            Err(failure) => {
                extend_unique_attachments(&mut all_attachments, &failure.attachments);
                let stop = match failure.error {
                    frona_harness::ToolDispatchError::Stop(stop) => stop,
                    error => return Err(protocol_error(error)),
                };
                return stopped(
                    &mut conversation,
                    stop,
                    turn_text,
                    all_attachments,
                    last_reasoning,
                );
            }
        };
        for result in results {
            extend_unique_attachments(&mut all_attachments, &result.result().attachments);
            match result {
                PersistentToolOutcome::Pending(result) => pending.push(result.record),
                PersistentToolOutcome::Task(result) | PersistentToolOutcome::Continue(result) => {
                    if task_event.is_none() {
                        task_event = result.record.task_event;
                    }
                    if let Some(prompt) = result.record.system_prompt {
                        prompts.push(prompt);
                    }
                }
            }
        }
        if let Err(error) = execution.check_cancellation() {
            return stopped(
                &mut conversation,
                error.into_execution_stop().expect("cancellation stop"),
                turn_text,
                all_attachments,
                last_reasoning,
            );
        }
        if !pending.is_empty() {
            stop_host_wait(&mut conversation, "waiting for external tool result")?;
            let system_prompt = pending
                .last()
                .and_then(|record| record.system_prompt.clone());
            return Ok(ToolLoopOutcome::ExternalToolPending {
                turn_text,
                tool_calls: pending,
                system_prompt,
            });
        }
        if ctx.shutdown_token.is_cancelled() {
            return shutdown(
                &mut conversation,
                turn_text,
                all_attachments,
                last_reasoning,
            );
        }
        if task_event.is_some() {
            conversation.finish().map_err(protocol_error)?;
            return Ok(ToolLoopOutcome::Completed {
                text: turn_text,
                attachments: all_attachments,
                lifecycle_event: task_event,
                reasoning: last_reasoning,
            });
        }
        for prompt in prompts {
            conversation
                .push_user(vec![rig_core::completion::message::UserContent::text(
                    prompt,
                )])
                .map_err(protocol_error)?;
        }
    }
    stopped(
        &mut conversation,
        ExecutionStop::Stopped("maximum tool turns reached".into()),
        last_text,
        all_attachments,
        last_reasoning,
    )
}

fn protocol_error(error: impl std::fmt::Display) -> AppError {
    AppError::Internal(format!("runtime conversation: {error}"))
}

fn stop_host_wait(
    conversation: &mut frona_harness::Conversation,
    message: &str,
) -> Result<(), AppError> {
    match conversation.stop_execution(frona_harness::ExecutionStop::Stopped(message.into())) {
        frona_harness::ToolDispatchError::Stop(frona_harness::ExecutionStop::Stopped(_)) => Ok(()),
        frona_harness::ToolDispatchError::Stop(stop) => Err(AppError::Inference(Box::new(
            frona_harness::inference::InferenceError::Stop(stop),
        ))),
        error => Err(protocol_error(error)),
    }
}

fn shutdown(
    conversation: &mut frona_harness::Conversation,
    text: String,
    attachments: Vec<crate::storage::Attachment>,
    reasoning: Option<Reasoning>,
) -> Result<ToolLoopOutcome, AppError> {
    stop_host_wait(conversation, "server shutting down")?;
    Ok(ToolLoopOutcome::Shutdown {
        text,
        attachments,
        reasoning,
    })
}

fn stopped(
    conversation: &mut frona_harness::Conversation,
    reason: frona_harness::ExecutionStop,
    text: String,
    attachments: Vec<crate::storage::Attachment>,
    reasoning: Option<Reasoning>,
) -> Result<ToolLoopOutcome, AppError> {
    let reason = match conversation.stop_execution(reason) {
        frona_harness::ToolDispatchError::Stop(reason) => reason,
        error => return Err(protocol_error(error)),
    };
    if matches!(reason, frona_harness::ExecutionStop::Cancelled(_)) {
        // Existing UI cancellation projection; the shared state and report remain incomplete.
        Ok(ToolLoopOutcome::Cancelled(text))
    } else {
        Ok(ToolLoopOutcome::Incomplete {
            reason,
            text,
            attachments,
            reasoning,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::AssistantContent;

    #[test]
    fn bridge_model_definitions_offer_the_wrapper_without_native_mcp_expansion() {
        let definitions = ["mcpctl", "mcp__demo__echo", "read"].map(|name| ToolDefinition {
            id: name.into(),
            provider_id: String::new(),
            description: String::new(),
            parameters: serde_json::json!({"type":"object"}),
        });
        let bridge = to_rig_tool_definitions(&definitions, true);
        assert_eq!(
            bridge
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["mcpctl", "read"]
        );
        let native = to_rig_tool_definitions(&definitions[1..], false);
        assert_eq!(
            native
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["mcp__demo__echo", "read"]
        );
    }

    #[test]
    fn native_definitions_are_identical_after_catalog_and_nested_key_reordering() {
        let a = ToolDefinition {
            id: "z".into(), provider_id: String::new(), description: "test".into(),
            parameters: serde_json::from_str(r#"{"type":"object","properties":{"b":{"type":"string","description":"B"},"a":{"type":"number"}}}"#).unwrap(),
        };
        let mut b = a.clone();
        b.id = "a".into();
        let first = to_rig_tool_definitions(&[a.clone(), b.clone()], false);
        let mut reordered = a;
        reordered.parameters = serde_json::from_str(r#"{"properties":{"a":{"type":"number"},"b":{"description":"B","type":"string"}},"type":"object"}"#).unwrap();
        let second = to_rig_tool_definitions(&[b, reordered], false);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    #[test]
    fn extract_reasoning_from_contents() {
        let contents = vec![
            AssistantContent::Reasoning(rig_core::message::Reasoning::new("thinking hard"))
                .with_native(serde_json::json!({"type":"thinking", "thinking":"thinking hard", "signature":"sig-123", "id":"r-1"})),
            AssistantContent::text("final answer"),
        ];
        let r = extract_reasoning(&contents);
        assert!(r.is_some());
        let r = r.unwrap();
        assert_eq!(r.content, "thinking hard");
        assert_eq!(r.id, Some("r-1".to_string()));
        assert_eq!(r.signature, Some("sig-123".to_string()));
    }

    #[test]
    fn extract_reasoning_returns_none_when_absent() {
        let contents = vec![AssistantContent::text("just text")];
        assert!(extract_reasoning(&contents).is_none());
    }

    #[test]
    fn extract_reasoning_joins_multi_chunk() {
        let contents = vec![AssistantContent::Reasoning(
            rig_core::message::Reasoning::new("chunk1 \nchunk2"),
        )];
        let r = extract_reasoning(&contents).unwrap();
        assert_eq!(r.content, "chunk1 \nchunk2");
    }

    #[test]
    fn extract_reasoning_preserves_opaque_blocks_when_display_text_is_empty() {
        let AssistantContent::Reasoning(reasoning) = AssistantContent::Reasoning(rig_core::message::Reasoning::new(""))
            .with_native(serde_json::json!({"type":"reasoning", "id":"rs_opaque", "encrypted_content":"ciphertext", "summary":[]})) else { unreachable!() };
        let contents = vec![AssistantContent::Reasoning(reasoning.clone())];

        let stored = extract_reasoning(&contents).expect("reasoning should be retained");

        assert_eq!(stored.id.as_deref(), Some("rs_opaque"));
        assert!(stored.content.is_empty());
        let replayed: rig_core::completion::message::Reasoning =
            serde_json::from_value(stored.raw.expect("raw reasoning should be stored")).unwrap();
        assert_eq!(replayed, reasoning);
    }
}
