//! **Structured inference** - getting a typed `T` out of a model instead of prose.
//!
//! Three shapes, in increasing order of how much rope the model gets:
//!   - [`structured_inference`] - one shot, no tools: schema in, `T` out.
//!   - [`structured_inference_with_tools`] - an agentic loop: the model may call read
//!     tools to investigate, then calls `submit` exactly once. The loop is ours.
//!   - [`StructuredConversation`] - a reusable dialogue that hides exploration and returns
//!     one typed answer attempt at a time. An external validator can reject a valid
//!     submission in the same dialogue and ask for another attempt.
//!
//! All three are **non-persistent**: nothing reaches the chat/message tables, only usage
//! metrics. (Not to be confused with [`crate::inference::conversation`], which builds the persistent
//! chat history.) Each terminates on a `submit` tool call carrying `T`'s JSON schema.

use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};

use crate::core::error::AppError;
use crate::tool::registry::AgentToolRegistry;

use crate::inference::InferenceContext;
use crate::inference::tool_loop;
use crate::inference::usage::{UsageContext, UsageService};
use frona_harness::inference::{
    InferenceError, ModelGroup, provider, submission::deserialize_submission,
};
use frona_harness::submission_tool;

pub async fn structured_inference<T>(
    model_group: &ModelGroup,
    system_prompt: &str,
    history: Vec<RigMessage>,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
) -> Result<T, InferenceError>
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
{
    let schema = serde_json::to_value(schemars::schema_for!(T))
        .map_err(|e| InferenceError::InferenceFailed(format!("schema_for failed: {e}")))?;
    let value = (model_group)
        .structured_inference(
            frona_harness::inference::ModelRequest {
                input_token_estimate: None,
                system_prompt,
                history,
                tools: vec![],
                observations: frona_harness::inference::observation::Observations::new(
                    &usage_service.scoped(usage_ctx),
                ),
                overrides: Default::default(),
            },
            schema,
        )
        .await?;
    // Same tolerance and the same diagnosis as the conversational path: a wrapper is a
    // wrapper whether or not the caller drives the loop.
    deserialize_submission::<T>(value)
        .map_err(|e| InferenceError::InferenceFailed(format!("submit args: {e}")))
}

/// Structured completion via an agentic tool loop: the model may call read tools to
/// investigate, then calls `submit` exactly once to return the typed result `T`.
/// Loops up to `max_turns` (a `submit` terminates early; text without a submit gives
/// up). The `InferenceContext` carries the identity + sandbox the tools run under.
#[allow(clippy::too_many_arguments)]
pub async fn structured_inference_with_tools<T>(
    model_group: &ModelGroup,
    system_prompt: &str,
    mut chat_history: Vec<RigMessage>,
    tool_registry: &AgentToolRegistry,
    ctx: &InferenceContext,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
    max_turns: usize,
) -> Result<T, AppError>
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
{
    let schema = serde_json::to_value(schemars::schema_for!(T))
        .map_err(|e| AppError::Internal(format!("structured_with_tools schema: {e}")))?;
    let submit = RigToolDefinition {
        name: provider::SUBMIT_TOOL_NAME.to_string(),
        description:
            "Submit the final structured result. Call this exactly once when you are done."
                .to_string(),
        parameters: schema,
    };
    let mut tool_defs = tool_loop::to_rig_tool_definitions(
        tool_registry.definitions(),
        tool_registry.mcp_bridge_mode(),
    );
    tool_defs.push(submit);

    for _ in 0..max_turns.max(1) {
        let frona_harness::inference::ModelResponse {
            content: contents, ..
        } = (model_group)
            .inference(frona_harness::inference::ModelRequest {
                input_token_estimate: None,
                system_prompt,
                history: chat_history.clone(),
                tools: tool_defs.clone(),
                observations: frona_harness::inference::observation::Observations::new(
                    &usage_service.scoped(usage_ctx),
                ),
                overrides: Default::default(),
            })
            .await
            .map_err(|e| AppError::Internal(format!("structured_with_tools inference: {e}")))?;

        // A `submit` call terminates the loop - its arguments are the result `T`.
        for content in &contents {
            if let AssistantContent::ToolCall(tc) = content
                && tc.function.name == provider::SUBMIT_TOOL_NAME
            {
                return deserialize_submission::<T>(tc.function.arguments.clone())
                    .map_err(|e| AppError::Internal(format!("structured_with_tools submit: {e}")));
            }
        }

        // Otherwise: append the assistant message, then run the (non-submit) tool
        // calls and feed their results back for the next turn.
        let has_tool_calls = tool_loop::process_model_response(&contents, &mut chat_history).await;
        if !has_tool_calls {
            break; // text without submitting -> give up (caller defaults to safe)
        }
        for content in &contents {
            if let AssistantContent::ToolCall(tc) = content {
                if tc.function.name == provider::SUBMIT_TOOL_NAME {
                    continue;
                }
                let text = match tool_registry
                    .execute(&tc.function.name, tc.function.arguments.clone(), ctx)
                    .await
                {
                    Ok(out) => out.text_content().to_string(),
                    Err(e) => format!("tool error: {e}"),
                };
                chat_history.push(RigMessage::User {
                    content: vec![rig_core::completion::message::UserContent::ToolResult(
                        rig_core::completion::message::ToolResult {
                            call: tc.id.clone(),
                            provider: tc.provider.clone(),
                            name: tc.function.name.clone(),
                            content: vec![rig_core::completion::message::ToolResultContent::text(
                                &text,
                            )],
                        },
                    )],
                });
            }
        }
    }
    Err(AppError::Internal(
        "structured_with_tools: model did not submit within max_turns".into(),
    ))
}

#[allow(clippy::too_many_arguments)]
pub async fn text_inference_with_tools(
    model_group: &ModelGroup,
    system_prompt: &str,
    mut chat_history: Vec<RigMessage>,
    tool_registry: &AgentToolRegistry,
    ctx: &InferenceContext,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
    max_turns: usize,
) -> Result<String, AppError> {
    let tool_defs = tool_loop::to_rig_tool_definitions(
        tool_registry.definitions(),
        tool_registry.mcp_bridge_mode(),
    );
    for _ in 0..max_turns.max(1) {
        let frona_harness::inference::ModelResponse {
            content: contents, ..
        } = (model_group)
            .inference(frona_harness::inference::ModelRequest {
                input_token_estimate: None,
                system_prompt,
                history: chat_history.clone(),
                tools: tool_defs.clone(),
                observations: frona_harness::inference::observation::Observations::new(
                    &usage_service.scoped(usage_ctx),
                ),
                overrides: Default::default(),
            })
            .await
            .map_err(|e| AppError::Internal(format!("text_with_tools inference: {e}")))?;
        let has_tool_calls = contents
            .iter()
            .any(|content| matches!(content, AssistantContent::ToolCall(_)));
        if !has_tool_calls {
            return provider::extract_text_from_choice(&contents)
                .map_err(|e| AppError::Internal(format!("text_with_tools response: {e}")));
        }
        tool_loop::process_model_response(&contents, &mut chat_history).await;
        for content in &contents {
            let AssistantContent::ToolCall(tc) = content else {
                continue;
            };
            let text = match tool_registry
                .execute(&tc.function.name, tc.function.arguments.clone(), ctx)
                .await
            {
                Ok(out) => out.text_content().to_string(),
                Err(e) => format!("tool error: {e}"),
            };
            chat_history.push(RigMessage::User {
                content: vec![rig_core::completion::message::UserContent::ToolResult(
                    rig_core::completion::message::ToolResult {
                        call: tc.id.clone(),
                        provider: tc.provider.clone(),
                        name: tc.function.name.clone(),
                        content: vec![rig_core::completion::message::ToolResultContent::text(
                            &text,
                        )],
                    },
                )],
            });
        }
    }
    Err(AppError::Internal(
        "text_with_tools: model did not finish within max_turns".into(),
    ))
}

/// One answer attempt after any internal exploration tool turns have completed.
#[derive(Debug)]
pub enum AnswerAttempt<T> {
    /// The model called `submit` with arguments that decoded as `T`.
    Submitted(T),
    /// The model called `submit`, but its arguments did not decode as `T`. The schema
    /// correction is already present in the conversation history.
    InvalidSubmission,
    /// The model answered without exploration tools or `submit`. The instruction to call
    /// `submit` is already present in the conversation history.
    MissingSubmission,
}

/// A **non-persistent, structured tool dialogue** that yields answer attempts for `T`.
/// [`next_attempt`](Self::next_attempt) hides exploration tool turns. A caller that rejects
/// a valid submission can use [`reject_submission`](Self::reject_submission) to request a
/// revision in the same in-memory conversation. Nothing is written to the chat/message
/// tables; only usage metrics are recorded. Built by `Harness::structured_conversation`.
pub struct StructuredConversation<'a, T> {
    usage_service: &'a UsageService,
    tools: AgentToolRegistry,
    ctx: InferenceContext,
    exploration_tool_defs: Vec<RigToolDefinition>,
    submit_tool_def: RigToolDefinition,
    history: Vec<RigMessage>,
    model_group: ModelGroup,
    system: String,
    usage_ctx: UsageContext,
    tool_turns_left: usize,
    requests_used: usize,
    last_submit: Option<(
        rig_core::completion::message::ToolCallId,
        Option<rig_core::completion::message::ProviderCallId>,
    )>,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<'a, T> StructuredConversation<'a, T>
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
{
    /// Assemble a conversation seeded with `system` + `initial`. `max_tool_turns` bounds
    /// exploration only. Answer attempts use a caller-owned limit.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        usage_service: &'a UsageService,
        tools: AgentToolRegistry,
        ctx: InferenceContext,
        model_group: ModelGroup,
        system: String,
        initial: String,
        usage_ctx: UsageContext,
        max_tool_turns: usize,
    ) -> Self {
        let exploration_tool_defs =
            tool_loop::to_rig_tool_definitions(tools.definitions(), tools.mcp_bridge_mode());
        Self {
            usage_service,
            tools,
            ctx,
            exploration_tool_defs,
            submit_tool_def: submission_tool::<T>(),
            history: vec![RigMessage::user(initial)],
            model_group,
            system,
            usage_ctx,
            tool_turns_left: max_tool_turns,
            requests_used: 0,
            last_submit: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Return one answer attempt after running any internal exploration tool turns.
    /// Invalid and missing submissions already have their protocol correction in history
    /// when they are returned. Exploration stops at `max_tool_turns`; later requests offer
    /// only `submit`.
    pub async fn next_attempt(&mut self) -> Result<AnswerAttempt<T>, AppError> {
        if self.last_submit.is_some() {
            return Err(AppError::Internal(
                "conversation: the previous submission must be rejected before another attempt"
                    .into(),
            ));
        }

        loop {
            let exploration_allowed = self.tool_turns_left > 0;
            let mut tool_defs = if exploration_allowed {
                self.exploration_tool_defs.clone()
            } else {
                Vec::new()
            };
            tool_defs.push(self.submit_tool_def.clone());

            self.requests_used += 1;
            let frona_harness::inference::ModelResponse {
                content: contents, ..
            } = (self.model_group)
                .inference(frona_harness::inference::ModelRequest {
                    input_token_estimate: None,
                    system_prompt: &self.system,
                    history: self.history.clone(),
                    tools: tool_defs,
                    observations: frona_harness::inference::observation::Observations::new(
                        &self.usage_service.scoped(&self.usage_ctx),
                    ),
                    overrides: Default::default(),
                })
                .await
                .map_err(|e| AppError::Internal(format!("conversation inference: {e}")))?;

            let submit = contents.iter().find_map(|c| match c {
                AssistantContent::ToolCall(tc)
                    if tc.function.name == provider::SUBMIT_TOOL_NAME =>
                {
                    Some((
                        tc.id.clone(),
                        tc.provider.clone(),
                        tc.function.arguments.clone(),
                    ))
                }
                _ => None,
            });
            tool_loop::process_model_response(&contents, &mut self.history).await;

            let has_exploration = contents.iter().any(|content| {
                matches!(content, AssistantContent::ToolCall(tc)
                    if tc.function.name != provider::SUBMIT_TOOL_NAME)
            });

            for content in &contents {
                if let AssistantContent::ToolCall(tc) = content {
                    if tc.function.name == provider::SUBMIT_TOOL_NAME {
                        continue;
                    }
                    let text = if exploration_allowed {
                        match self
                            .tools
                            .execute(&tc.function.name, tc.function.arguments.clone(), &self.ctx)
                            .await
                        {
                            Ok(out) => out.text_content().to_string(),
                            Err(e) => format!("tool error: {e}"),
                        }
                    } else {
                        "tool error: exploration tool limit reached; call `submit` now".into()
                    };
                    self.history.push(RigMessage::User {
                        content: vec![rig_core::completion::message::UserContent::ToolResult(
                            rig_core::completion::message::ToolResult {
                                call: tc.id.clone(),
                                provider: tc.provider.clone(),
                                name: tc.function.name.clone(),
                                content: vec![
                                    rig_core::completion::message::ToolResultContent::text(&text),
                                ],
                            },
                        )],
                    });
                }
            }

            if has_exploration && exploration_allowed {
                self.tool_turns_left -= 1;
            }

            if let Some((id, call_id, args)) = submit {
                self.last_submit = Some((id, call_id));
                return match deserialize_submission::<T>(args) {
                    Ok(value) => Ok(AnswerAttempt::Submitted(value)),
                    Err(e) => {
                        tracing::warn!(error = %e, "structured conversation: malformed submission, asking for a correction");
                        self.answer_pending_submission(format!(
                            "Your `submit` call did not match the required schema and was NOT \
                             recorded: {e}. Call `submit` again with every required field present."
                        ))?;
                        Ok(AnswerAttempt::InvalidSubmission)
                    }
                };
            }

            if has_exploration && exploration_allowed {
                continue;
            }

            tracing::warn!("structured conversation: no submission, asking for one explicitly");
            self.history.push(RigMessage::user(format!(
                "You replied without a recorded submission. Call the `{}` tool with your answer.",
                provider::SUBMIT_TOOL_NAME
            )));
            return Ok(AnswerAttempt::MissingSubmission);
        }
    }

    /// Provider requests made by this dialogue, including exploration and answer attempts.
    /// This is diagnostic only.
    pub fn requests_used(&self) -> usize {
        self.requests_used
    }

    /// Reject the valid submission returned by the last call to `next_attempt`.
    pub fn reject_submission(&mut self, reason: impl Into<String>) -> Result<(), AppError> {
        self.answer_pending_submission(reason.into())
    }

    fn answer_pending_submission(&mut self, text: String) -> Result<(), AppError> {
        let Some((id, call_id)) = self.last_submit.take() else {
            return Err(AppError::Internal(
                "conversation: no submitted answer is waiting for feedback".into(),
            ));
        };
        self.history.push(RigMessage::User {
            content: vec![rig_core::completion::message::UserContent::ToolResult(
                rig_core::completion::message::ToolResult {
                    call: id,
                    provider: call_id,
                    name: provider::SUBMIT_TOOL_NAME.to_string(),
                    content: vec![rig_core::completion::message::ToolResultContent::text(
                        &text,
                    )],
                },
            )],
        });
        Ok(())
    }
}
