use base64::Engine;
use rig::completion::message::{
    DocumentSourceKind, ImageMediaType, MimeType, ToolResult, ToolResultContent, UserContent,
};
use rig::completion::request::ToolDefinition as RigToolDefinition;
use rig::completion::{AssistantContent, Message as RigMessage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::chat::message::models::MessageTool;
use crate::error::AppError;
use crate::tool::registry::AgentToolRegistry;
use crate::tool::{ToolContext, ToolDefinition};

use super::config::ModelGroup;
use super::registry::ModelProviderRegistry;

const MAX_TOOL_TURNS: usize = 20;
const RATE_LIMIT_BACKOFF_SECS: &[u64] = &[5, 10, 20, 40, 80, 160, 285];

pub struct ToolLoopEvent {
    pub kind: ToolLoopEventKind,
}

pub enum ToolLoopEventKind {
    Text(String),
    ToolCall {
        name: String,
        arguments: serde_json::Value,
        description: Option<String>,
    },
    ToolResult {
        name: String,
        result: String,
    },
    EntityUpdated {
        table: String,
        record_id: String,
        fields: serde_json::Value,
    },
    RateLimitRetry { retry_after_secs: u64 },
    Done(String),
    Cancelled(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: String,
    pub tool_data: Option<MessageTool>,
}

pub enum ToolLoopOutcome {
    Completed {
        text: String,
        attachments: Vec<crate::api::files::Attachment>,
    },
    Cancelled(String),
    ExternalToolPending {
        accumulated_text: String,
        tool_calls_json: serde_json::Value,
        tool_results: Vec<ToolCallResult>,
        external_tool: Box<ToolCallResult>,
    },
}

fn to_rig_tool_definitions(defs: &[ToolDefinition]) -> Vec<RigToolDefinition> {
    defs.iter()
        .map(|d| RigToolDefinition {
            name: d.name.clone(),
            description: d.description.clone(),
            parameters: d.parameters.clone(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tool_loop(
    registry: &ModelProviderRegistry,
    model_group: &ModelGroup,
    system_prompt: &str,
    mut chat_history: Vec<RigMessage>,
    tool_registry: &AgentToolRegistry,
    event_tx: mpsc::Sender<ToolLoopEvent>,
    cancel_token: CancellationToken,
    ctx: &ToolContext,
) -> Result<ToolLoopOutcome, AppError> {
    let tool_defs = &tool_registry.definitions;
    let rig_tools = to_rig_tool_definitions(tool_defs);

    let provider = registry
        .get_provider(&model_group.main.provider)
        .map_err(|e| AppError::Llm(e.to_string()))?;

    let mut accumulated_text = String::new();
    let mut all_attachments: Vec<crate::api::files::Attachment> = Vec::new();

    for turn in 0..MAX_TOOL_TURNS {
        if cancel_token.is_cancelled() {
            let _ = event_tx
                .send(ToolLoopEvent {
                    kind: ToolLoopEventKind::Cancelled(accumulated_text.clone()),
                })
                .await;
            return Ok(ToolLoopOutcome::Cancelled(accumulated_text));
        }

        tracing::debug!(turn, "Tool loop turn");

        let max_output = model_group.max_tokens.unwrap_or(8192) as usize;
        chat_history = crate::llm::context::truncate_history(
            chat_history,
            system_prompt,
            &model_group.main.model_id,
            model_group.context_window,
            max_output,
        );

        let contents: Vec<AssistantContent>;
        let mut rate_limit_attempt: usize = 0;

        loop {
            let (text_tx, text_rx) = mpsc::channel::<String>(32);

            let event_tx_clone = event_tx.clone();
            let forward_handle = tokio::spawn(async move {
                let mut text_rx = text_rx;
                let mut text = String::new();
                while let Some(token) = text_rx.recv().await {
                    text.push_str(&token);
                    let _ = event_tx_clone
                        .send(ToolLoopEvent {
                            kind: ToolLoopEventKind::Text(token),
                        })
                        .await;
                }
                text
            });

            let contents_result = tokio::select! {
                result = provider.stream_inference_with_tools(
                    &model_group.main.model_id,
                    system_prompt,
                    chat_history.clone(),
                    rig_tools.clone(),
                    text_tx,
                    model_group.max_tokens,
                    model_group.temperature,
                ) => Some(result),
                _ = cancel_token.cancelled() => None,
            };

            let turn_text = forward_handle.await.unwrap_or_default();
            accumulated_text.push_str(&turn_text);

            if contents_result.is_none() {
                let _ = event_tx
                    .send(ToolLoopEvent {
                        kind: ToolLoopEventKind::Cancelled(accumulated_text.clone()),
                    })
                    .await;
                return Ok(ToolLoopOutcome::Cancelled(accumulated_text));
            }

            match contents_result.unwrap() {
                Ok(result) => {
                    contents = result;
                    break;
                }
                Err(ref e) if e.is_rate_limited() => {
                    let delay = RATE_LIMIT_BACKOFF_SECS
                        .get(rate_limit_attempt)
                        .copied();

                    match delay {
                        Some(secs) => {
                            tracing::warn!(
                                retry_after_secs = secs,
                                attempt = rate_limit_attempt + 1,
                                "Rate limited, retrying"
                            );
                            let _ = event_tx
                                .send(ToolLoopEvent {
                                    kind: ToolLoopEventKind::RateLimitRetry {
                                        retry_after_secs: secs,
                                    },
                                })
                                .await;
                            rate_limit_attempt += 1;
                            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                        }
                        None => {
                            return Err(AppError::Llm(
                                "Rate limited: max retry time exceeded".to_string(),
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(AppError::Llm(e.to_string()));
                }
            }
        }

        let mut has_tool_calls = false;
        let mut assistant_content_items: Vec<AssistantContent> = Vec::new();

        for content in &contents {
            match content {
                AssistantContent::Text(_) => {
                    // Text already streamed via forward task
                }
                AssistantContent::ToolCall(tool_call) => {
                    has_tool_calls = true;
                    let mut args = tool_call.function.arguments.clone();
                    let description = args
                        .as_object_mut()
                        .and_then(|obj| obj.remove("description"))
                        .and_then(|v| v.as_str().map(String::from));
                    let _ = event_tx
                        .send(ToolLoopEvent {
                            kind: ToolLoopEventKind::ToolCall {
                                name: tool_call.function.name.clone(),
                                arguments: args,
                                description,
                            },
                        })
                        .await;
                }
                _ => {}
            }
            assistant_content_items.push(content.clone());
        }

        let assistant_msg = RigMessage::Assistant {
            id: None,
            content: rig::OneOrMany::many(assistant_content_items)
                .unwrap_or_else(|_| rig::OneOrMany::one(AssistantContent::text(""))),
        };
        chat_history.push(assistant_msg);

        if !has_tool_calls {
            let _ = event_tx
                .send(ToolLoopEvent {
                    kind: ToolLoopEventKind::Done(accumulated_text.clone()),
                })
                .await;
            break;
        }

        let mut has_external = false;
        let mut external_tool_result: Option<ToolCallResult> = None;
        let mut internal_tool_results: Vec<ToolCallResult> = Vec::new();

        for content in &contents {
            if let AssistantContent::ToolCall(tool_call) = content {
                let tool_name = &tool_call.function.name;
                let mut arguments = tool_call.function.arguments.clone();
                if let Some(obj) = arguments.as_object_mut() {
                    obj.remove("description");
                }

                let is_external = tool_registry.is_external(tool_name);

                tracing::debug!(tool = %tool_name, args = %arguments, external = is_external, "Executing tool");

                let (result, tool_output) = match tool_registry.execute(tool_name, arguments, ctx).await {
                    Ok(output) => {
                        let text = output.text_content().to_string();
                        tracing::debug!(tool = %tool_name, result = %text, "Tool executed");
                        (text, Some(output))
                    }
                    Err(e) => {
                        tracing::warn!(tool = %tool_name, error = %e, "Tool execution failed");
                        (format!("Error: {e}"), None)
                    }
                };

                let _ = event_tx
                    .send(ToolLoopEvent {
                        kind: ToolLoopEventKind::ToolResult {
                            name: tool_name.clone(),
                            result: result.clone(),
                        },
                    })
                    .await;

                let td = tool_output.as_ref().and_then(|o| o.tool_data().cloned());

                if let Some(ref output) = tool_output {
                    for attachment in output.attachments() {
                        all_attachments.push(attachment.clone());
                    }
                }

                let tool_result = ToolCallResult {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_name.clone(),
                    result: result.clone(),
                    tool_data: td,
                };

                if is_external {
                    has_external = true;
                    external_tool_result = Some(tool_result);
                } else {
                    internal_tool_results.push(tool_result);
                    let has_images = tool_output
                        .as_ref()
                        .is_some_and(|o| !o.images().is_empty());
                    let tool_result_msg = if has_images {
                        let output = tool_output.unwrap();
                        let tool_result_content =
                            UserContent::ToolResult(ToolResult {
                                id: tool_call.id.clone(),
                                call_id: None,
                                content: rig::OneOrMany::one(ToolResultContent::text(result)),
                            });
                        let mut user_contents = vec![tool_result_content];
                        for img in output.images() {
                            let b64 = base64::engine::general_purpose::STANDARD
                                .encode(&img.bytes);
                            user_contents.push(UserContent::Image(
                                rig::completion::message::Image {
                                    data: DocumentSourceKind::Base64(b64),
                                    media_type: ImageMediaType::from_mime_type(
                                        &img.media_type,
                                    ),
                                    detail: None,
                                    additional_params: None,
                                },
                            ));
                        }
                        RigMessage::User {
                            content: rig::OneOrMany::many(user_contents).unwrap(),
                        }
                    } else {
                        RigMessage::tool_result(tool_call.id.clone(), result)
                    };
                    chat_history.push(tool_result_msg);
                }
            }
        }

        if cancel_token.is_cancelled() {
            let _ = event_tx
                .send(ToolLoopEvent {
                    kind: ToolLoopEventKind::Cancelled(accumulated_text.clone()),
                })
                .await;
            return Ok(ToolLoopOutcome::Cancelled(accumulated_text));
        }

        if has_external {
            let external_tool = external_tool_result.unwrap();

            let tool_calls_json = contents
                .iter()
                .filter_map(|c| {
                    if let AssistantContent::ToolCall(tc) = c {
                        Some(serde_json::json!({
                            "id": tc.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            return Ok(ToolLoopOutcome::ExternalToolPending {
                accumulated_text,
                tool_calls_json: serde_json::json!(tool_calls_json),
                tool_results: internal_tool_results,
                external_tool: Box::new(external_tool),
            });
        }

        if turn == MAX_TOOL_TURNS - 1 {
            let _ = event_tx
                .send(ToolLoopEvent {
                    kind: ToolLoopEventKind::Error(
                        "Max tool turns reached".to_string(),
                    ),
                })
                .await;
        }
    }

    Ok(ToolLoopOutcome::Completed {
        text: accumulated_text,
        attachments: all_attachments,
    })
}
