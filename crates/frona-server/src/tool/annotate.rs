use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::signal::{Annotation, CandidateEvent, SignalService};
use crate::chat::service::ChatService;
use crate::contact::service::ContactService;
use crate::core::error::AppError;
use crate::space::service::SpaceService;
use crate::tool::{
    AgentTool, InferenceContext, ToolDefinition, ToolOutput, active_chat, load_tool_definition,
};
use frona_text::prompt::PromptLoader;

pub struct AnnotateTool {
    pub signal_service: Arc<SignalService>,
    pub chat_service: ChatService,
    pub space_service: SpaceService,
    pub contact_service: ContactService,
    pub channel_service: crate::chat::channel::ChannelService,
    pub prompts: PromptLoader,
}

impl AnnotateTool {
    pub fn new(
        signal_service: Arc<SignalService>,
        chat_service: ChatService,
        space_service: SpaceService,
        contact_service: ContactService,
        channel_service: crate::chat::channel::ChannelService,
        prompts: PromptLoader,
    ) -> Self {
        Self {
            signal_service,
            chat_service,
            space_service,
            contact_service,
            channel_service,
            prompts,
        }
    }
}

const MAX_CATEGORIES: usize = 32;

#[async_trait]
impl AgentTool for AnnotateTool {
    fn name(&self) -> &str {
        "annotate_message"
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        load_tool_definition(&self.prompts, "tools/annotate_message.md")
            .map(|d| vec![d])
            .unwrap_or_default()
    }

    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        let categories = parse_categories(&arguments)?;
        let summary = arguments
            .get("summary")
            .and_then(|v| v.as_str())
            .map(String::from);

        let annotator_id = format!("agent:{}", ctx.agent.id);
        let mut annotations: Vec<Annotation> = categories
            .into_iter()
            .map(|c| Annotation::category(annotator_id.clone(), c))
            .collect();
        if let Some(s) = summary {
            annotations.push(Annotation::summary(annotator_id.clone(), s));
        }

        let candidate = self.build_candidate(ctx, annotations).await?;
        let fired = self
            .signal_service
            .evaluate(&ctx.user.id, candidate)
            .await?;

        Ok(ToolOutput::text(if fired.is_empty() {
            "Annotated. No pending signals matched.".to_string()
        } else {
            format!("Annotated. Fired {} signal(s).", fired.len())
        }))
    }
}

impl AnnotateTool {
    async fn build_candidate(
        &self,
        ctx: &InferenceContext,
        annotations: Vec<Annotation>,
    ) -> Result<CandidateEvent, AppError> {
        let chat = active_chat(ctx)?;
        let message = self
            .chat_service
            .get_stored_messages(&chat.id)
            .await?
            .into_iter()
            .rev()
            .find(|m| {
                matches!(
                    m.role,
                    crate::chat::message::models::MessageRole::User
                        | crate::chat::message::models::MessageRole::System
                )
            });

        let sender = message.as_ref().and_then(|m| m.from_address.clone());
        let content = message
            .as_ref()
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let channel = if let Some(space_id) = chat.space_id.as_deref() {
            self.channel_service
                .find_by_space(space_id)
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        let contact = if let Some(addr) = sender.as_deref() {
            self.contact_service
                .list(&ctx.user.id)
                .await
                .ok()
                .and_then(|cs| {
                    cs.into_iter().find(|c| {
                        c.phone.as_deref() == Some(addr) || c.email.as_deref() == Some(addr)
                    })
                })
        } else {
            None
        };

        Ok(CandidateEvent {
            channel,
            chat: Some(chat.clone()),
            message,
            contact,
            sender,
            annotations,
            content,
        })
    }
}

fn parse_categories(arguments: &Value) -> Result<Vec<String>, AppError> {
    let arr = arguments
        .get("categories")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppError::Validation("categories must be an array".into()))?;
    if arr.is_empty() {
        return Err(AppError::Validation(
            "categories must contain at least one entry".into(),
        ));
    }
    if arr.len() > MAX_CATEGORIES {
        return Err(AppError::Validation(format!(
            "categories must contain at most {MAX_CATEGORIES} entries"
        )));
    }
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let s = v
            .as_str()
            .ok_or_else(|| AppError::Validation("category entries must be strings".into()))?
            .trim();
        if s.is_empty() {
            return Err(AppError::Validation(
                "category entries must be non-empty".into(),
            ));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_categories_rejects_empty() {
        assert!(parse_categories(&json!({"categories": []})).is_err());
    }

    #[test]
    fn parse_categories_rejects_missing() {
        assert!(parse_categories(&json!({})).is_err());
    }

    #[test]
    fn parse_categories_rejects_non_string_entries() {
        assert!(parse_categories(&json!({"categories": [42]})).is_err());
    }

    #[test]
    fn parse_categories_rejects_blank_entries() {
        assert!(parse_categories(&json!({"categories": ["", "ok"]})).is_err());
    }

    #[test]
    fn parse_categories_trims_and_collects() {
        let cats = parse_categories(&json!({"categories": ["  one ", "two"]})).unwrap();
        assert_eq!(cats, vec!["one".to_string(), "two".to_string()]);
    }
}
