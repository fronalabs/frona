//! One bounded summarization attempt before a shared session model call.

use rig_core::completion::{AssistantContent, Message, request::ToolDefinition};

use super::*;
use crate::{CompactionError, CompactionLimits, HistoryView, RequestInput};

pub(super) const SUMMARY_INSTRUCTION: &str = "Summarize the earlier work for continuation. Preserve facts, constraints, evidence, failed approaches and unresolved work. Return concise plain text only; do not call tools or answer the task.";

impl SessionCore<'_> {
    pub(super) async fn prepare_model_view(
        &mut self,
        model: &Model<'_>,
        tools: &[ToolDefinition],
    ) -> Result<(Vec<Message>, RequestInput, Option<u64>), ManualError> {
        let mut config = self.compaction;
        if !self.compaction_configured {
            // A manual call can select a different prepared group. Default
            // capacity follows that selection; explicit application policy does not.
            config.context_window = model.compaction_config().context_window;
        }
        let (max_tokens, request_max_tokens) = model.output_reservation(config.max_output_tokens);
        let output_tokens = usize::try_from(max_tokens).ok();
        if config.target_percent == 0
            || config.target_percent >= config.trigger_percent
            || config.trigger_percent > 100
            || max_tokens == 0
            || output_tokens.is_none_or(|output| output >= config.context_window)
        {
            return Err(self.fail(CompactionError::InvalidConfiguration.into()));
        }
        let available = config.context_window - output_tokens.unwrap();
        let fraction = |percent: u8| ((available as u128 * percent as u128) / 100) as usize;
        let view = self.conversation.model_view().map_err(ManualError::from)?;
        let before = view.estimated_tokens(&self.instructions, tools);
        if !config.enabled || before <= fraction(config.trigger_percent) {
            if before > available {
                return Err(self.fail(
                    CompactionError::ContextOverflow {
                        estimated: before,
                        available,
                    }
                    .into(),
                ));
            }
            return Ok((
                view.messages(),
                self.conversation.request_input(view.original_positions()),
                request_max_tokens,
            ));
        }
        let plan = match view.plan(
            &self.instructions,
            tools,
            CompactionLimits {
                available_input_tokens: available,
                target_input_tokens: fraction(config.target_percent).max(1),
            },
        ) {
            Ok(Some(plan)) => plan,
            Ok(None) => {
                return Ok((
                    view.messages(),
                    self.conversation.request_input(view.original_positions()),
                    request_max_tokens,
                ));
            }
            Err(error) => return Err(self.fail(error.into())),
        };
        let mut messages = plan.summary_input().to_vec();
        messages.push(Message::user(SUMMARY_INSTRUCTION));
        let estimated = HistoryView::new(&messages).estimated_tokens(&self.instructions, &[]);
        if estimated > available {
            return Err(self.fail(
                CompactionError::ContextOverflow {
                    estimated,
                    available,
                }
                .into(),
            ));
        }
        let input = self
            .conversation
            .request_input(plan.summary_input_original_positions().to_vec());
        let request = ModelRequest {
            system: self.instructions.clone(),
            messages,
            tools: Vec::new(),
            schema: None,
            max_tokens: request_max_tokens,
            temperature: None,
        };
        // No application tools are offered or dispatched by this generation.
        // It has its own logical request/attempt, using the same budget object.
        let invocation = match Self::invoke(
            model,
            request,
            self.execution(&input),
            self.progress.publisher(),
            false,
            self.observations(),
        )
        .await
        {
            Ok(output) => output,
            Err(ModelError::Provider(error)) => {
                let error = match error.into_execution_stop() {
                    Ok(stop) => stop.into(),
                    Err(error) => ModelError::Provider(error).into(),
                };
                return Err(self.fail(error));
            }
            Err(error) => return Err(self.fail(error.into())),
        };
        let output = invocation.output;
        if output.structured.is_some()
            || output
                .content
                .iter()
                .any(|c| matches!(c, AssistantContent::ToolCall(_)))
        {
            return Err(self.fail(CompactionError::InvalidSummary.into()));
        }
        let summary = output
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let compacted = match plan.finish(&summary, None) {
            Ok(view) => view,
            Err(error) => return Err(self.fail(error.into())),
        };
        let after = HistoryView::from_compacted(self.conversation.messages(), &compacted)?
            .estimated_tokens(&self.instructions, tools);
        if after >= before {
            return Err(self.fail(CompactionError::NoReduction { before, after }.into()));
        }
        let result = self
            .conversation
            .commit_compaction(compacted, invocation.origin, output);
        self.result(result)?;
        // A writer may cancel synchronously. The committed view stays durable,
        // but no main request may follow cancellation or a failed required write.
        self.check_active()?;
        let view = self.conversation.model_view()?;
        let input = self.conversation.request_input(view.original_positions());
        Ok((view.messages(), input, request_max_tokens))
    }
}
