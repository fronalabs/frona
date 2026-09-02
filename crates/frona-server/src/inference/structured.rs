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
//! metrics. (Not to be confused with [`super::conversation`], which builds the persistent
//! chat history.) Each terminates on a `submit` tool call carrying `T`'s JSON schema.

use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};

use crate::core::error::AppError;
use crate::tool::registry::AgentToolRegistry;

use super::config::ModelGroup;
use super::usage::{UsageContext, UsageService};
use super::{InferenceContext, InferenceError, ModelProviderRegistry, provider, retry, tool_loop};

pub async fn structured_inference<T>(
    registry: &ModelProviderRegistry,
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
    let value = retry::structured_inference_with_retry_and_fallback(
        registry,
        model_group,
        system_prompt,
        history,
        schema,
        usage_service,
        usage_ctx,
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
    registry: &ModelProviderRegistry,
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
        let (contents, _usage) = retry::inference_with_retry_and_fallback(
            registry,
            model_group,
            system_prompt,
            chat_history.clone(),
            tool_defs.clone(),
            usage_service,
            usage_ctx,
        )
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
    registry: &ModelProviderRegistry,
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
        let (contents, _usage) = retry::inference_with_retry_and_fallback(
            registry,
            model_group,
            system_prompt,
            chat_history.clone(),
            tool_defs.clone(),
            usage_service,
            usage_ctx,
        )
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

/// Turn a `submit` call's arguments into `T`, tolerating one spurious wrapper level, and
/// on failure describing the payload rather than just naming a field.
///
/// **The wrapper.** Models sometimes nest the answer one level down -
/// `{"result": {"classes": [...]}}` - measured at 4 of 43 submits against DeepSeek. The
/// schema is not consulted to detect it: `T` is already known, so each candidate is simply
/// handed to serde and the first that deserializes wins. That needs no list of blessed
/// wrapper names and cannot mis-read a real payload, because the only thing deciding is
/// `T`'s own impl. Only a **single-key** object is a candidate, so an envelope is
/// distinguishable from an answer that happens to have one field.
///
/// **The message.** `missing field \`classes\`` is true of the value serde was handed and
/// actively misleading about the value the model *sent*: it did include `classes`, one
/// level down. Observed consequence - the model answers
/// *"the error says missing field `classes` but I clearly included `classes`"*, goes looking
/// for a fault inside `classes`, finds none, and burns the turn budget. Naming the keys
/// that were actually present is what makes the failure self-diagnosing.
fn deserialize_submission<T>(args: serde_json::Value) -> Result<T, String>
where
    T: schemars::JsonSchema + serde::de::DeserializeOwned,
{
    let first = match serde_json::from_value::<T>(args.clone()) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    if let Some(inner) = args
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.values().next())
        && let Ok(v) = serde_json::from_value::<T>(inner.clone())
    {
        let key = args
            .as_object()
            .and_then(|o| o.keys().next())
            .cloned()
            .unwrap_or_default();
        tracing::debug!(wrapper = %key, "unwrapped a submission nested one level down");
        return Ok(v);
    }
    let validator = submission_validator::<T>();
    if let Some(repaired) = validator
        .as_ref()
        .and_then(|validator| repair_json_text_fields(&args, validator))
        && let Ok(v) = serde_json::from_value::<T>(repaired)
    {
        tracing::debug!("decoded JSON text in structured submission fields");
        return Ok(v);
    }
    Err(format!(
        "{first}{}",
        describe_payload(&args, validator.as_ref())
    ))
}

fn submission_validator<T: schemars::JsonSchema>() -> Option<jsonschema::Validator> {
    let schema = serde_json::to_value(schemars::schema_for!(T)).ok()?;
    jsonschema::validator_for(&schema).ok()
}

/// Decode model-authored arrays or objects that arrived as JSON inside a string. A schema type
/// error selects the field, and the complete repaired payload must still deserialize as `T`.
fn repair_json_text_fields(
    args: &serde_json::Value,
    validator: &jsonschema::Validator,
) -> Option<serde_json::Value> {
    let repairs = validator
        .iter_errors(args)
        .filter_map(|error| {
            let jsonschema::error::ValidationErrorKind::Type { kind } = error.kind() else {
                return None;
            };
            let serde_json::Value::String(text) = error.instance().as_ref() else {
                return None;
            };
            let parsed = serde_json::from_str::<serde_json::Value>(text).ok()?;
            let parsed_type = match parsed {
                serde_json::Value::Array(_) => jsonschema::JsonType::Array,
                serde_json::Value::Object(_) => jsonschema::JsonType::Object,
                _ => return None,
            };
            let expected_type = match kind {
                jsonschema::error::TypeKind::Single(expected) => *expected == parsed_type,
                jsonschema::error::TypeKind::Multiple(expected) => expected.contains(parsed_type),
            };
            expected_type.then(|| (error.instance_path().to_string(), parsed))
        })
        .collect::<Vec<_>>();
    if repairs.is_empty() {
        return None;
    }

    let mut repaired = args.clone();
    for (path, value) in repairs {
        *repaired.pointer_mut(&path)? = value;
    }
    Some(repaired)
}

/// A short, factual description of what the model actually sent, appended to a serde error.
fn describe_payload(args: &serde_json::Value, validator: Option<&jsonschema::Validator>) -> String {
    match args {
        serde_json::Value::Object(o) if o.is_empty() => " - you sent an empty object".into(),
        serde_json::Value::Object(o) => {
            let keys: Vec<&str> = o.keys().map(String::as_str).collect();
            let Some(error) = validator.and_then(|validator| validator.iter_errors(args).next())
            else {
                return format!(
                    " - the object you sent has these keys: [{}].",
                    keys.join(", ")
                );
            };
            let path = error.instance_path().to_string();
            if path.is_empty()
                && matches!(
                    error.kind(),
                    jsonschema::error::ValidationErrorKind::Required { .. }
                )
            {
                format!(
                    " - the object you sent has these keys: [{}]. Put the required fields at the \
                     TOP level of the `submit` arguments, not nested inside another key.",
                    keys.join(", ")
                )
            } else if path.is_empty() {
                format!(
                    " - the submit arguments do not match the schema: {error}. The object you \
                     sent has these keys: [{}].",
                    keys.join(", ")
                )
            } else {
                format!(" - field `{path}` does not match the submit schema: {error}.")
            }
        }
        other => format!(
            " - you sent a {}, but `submit` takes an object whose keys are the required fields.",
            match other {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => unreachable!("handled above"),
            }
        ),
    }
}

/// The `submit` tool definition that terminates a caller-driven structured loop,
/// built from `T`'s JSON schema (see [`StructuredConversation`]).
fn submit_tool_definition<T: schemars::JsonSchema>() -> RigToolDefinition {
    let parameters = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default();
    RigToolDefinition {
        name: provider::SUBMIT_TOOL_NAME.to_string(),
        description: "Submit the final structured result. Call this once you are confident."
            .to_string(),
        parameters,
    }
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
    registry: &'a ModelProviderRegistry,
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
        registry: &'a ModelProviderRegistry,
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
            registry,
            usage_service,
            tools,
            ctx,
            exploration_tool_defs,
            submit_tool_def: submit_tool_definition::<T>(),
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
            let (contents, _usage) = retry::inference_with_retry_and_fallback(
                self.registry,
                &self.model_group,
                &self.system,
                self.history.clone(),
                tool_defs,
                self.usage_service,
                &self.usage_ctx,
            )
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

#[cfg(test)]
mod submission_tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Deserialize, schemars::JsonSchema)]
    struct Classification {
        classes: Vec<String>,
        #[serde(default)]
        relations: Vec<String>,
    }

    /// The measured failure: the answer is one level down under a key the model invented.
    #[test]
    fn a_submission_nested_under_a_wrapper_key_is_unwrapped() {
        for key in [
            "result",
            "data",
            "output",
            "classification",
            "anything_at_all",
        ] {
            let args = serde_json::json!({ key: { "classes": ["schema:Person"] } });
            let got: Classification = deserialize_submission(args).expect(key);
            assert_eq!(
                got.classes,
                ["schema:Person"],
                "wrapper `{key}` not unwrapped"
            );
        }
    }

    /// No allow-list of wrapper names: `T` deciding is the whole mechanism, so a key nobody
    /// anticipated works exactly as well as `result`.
    #[test]
    fn a_correct_submission_is_taken_as_is() {
        let args = serde_json::json!({ "classes": ["schema:Person"], "relations": ["a"] });
        let got: Classification = deserialize_submission(args).unwrap();
        assert_eq!(
            got,
            Classification {
                classes: vec!["schema:Person".into()],
                relations: vec!["a".into()]
            }
        );
    }

    /// A single-key object that *is* the answer must not be mistaken for an envelope. Here
    /// the top level deserializes, so unwrapping is never attempted.
    #[test]
    fn a_one_field_answer_is_not_mistaken_for_a_wrapper() {
        let args = serde_json::json!({ "classes": ["schema:Person"] });
        let got: Classification = deserialize_submission(args).unwrap();
        assert_eq!(got.classes, ["schema:Person"]);
        assert!(got.relations.is_empty());
    }

    /// Unwrapping is one level only, and only when the inner value actually deserializes -
    /// otherwise a wrong guess would be reported as success.
    #[test]
    fn a_wrapper_whose_contents_are_wrong_is_still_an_error() {
        let args = serde_json::json!({ "result": { "not_classes": [] } });
        let err = deserialize_submission::<Classification>(args).expect_err("must not succeed");
        assert!(err.contains("classes"), "{err}");
    }

    #[test]
    fn double_wrapping_is_not_unwrapped() {
        let args = serde_json::json!({ "a": { "b": { "classes": [] } } });
        assert!(deserialize_submission::<Classification>(args).is_err());
    }

    /// The diagnosis. `missing field \`classes\`` alone sent the model hunting inside
    /// `classes`; the keys it actually sent are what let it find the real fault.
    #[test]
    fn the_error_names_the_keys_the_model_actually_sent() {
        let args = serde_json::json!({ "result": { "not_classes": [] } });
        let err = deserialize_submission::<Classification>(args).unwrap_err();
        assert!(err.contains("[result]"), "names the offending key: {err}");
        assert!(err.contains("TOP level"), "says what to do about it: {err}");
    }

    #[test]
    fn the_error_calls_out_a_non_object_payload_by_type() {
        let err = deserialize_submission::<Classification>(serde_json::json!([1, 2])).unwrap_err();
        assert!(err.contains("array"), "{err}");
        let err = deserialize_submission::<Classification>(serde_json::json!("hi")).unwrap_err();
        assert!(err.contains("string"), "{err}");
    }

    #[test]
    fn an_empty_object_is_reported_as_empty() {
        let err = deserialize_submission::<Classification>(serde_json::json!({})).unwrap_err();
        assert!(err.contains("empty object"), "{err}");
    }
}
