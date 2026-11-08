//! Native one-shot structured generation for persistent integrations.
//!
//! This primitive delegates provider retry/fallback and admission to ModelGroup,
//! then decodes its result. It owns no tool/refinement loop. New application loops
//! use `frona_harness::Harness`; signal processing retains this native-schema path.

use crate::inference::usage::{UsageContext, UsageService};
use frona_harness::inference::{InferenceError, ModelGroup, submission::deserialize_submission};
use rig_core::completion::Message as RigMessage;

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
