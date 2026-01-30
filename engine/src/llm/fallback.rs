use backon::Retryable;
use rig::completion::Message as RigMessage;
use tokio::sync::mpsc;

use super::config::{ModelGroup, RetryConfig};
use super::context::truncate_history;
use super::error::LlmError;
use super::registry::ModelProviderRegistry;

pub async fn inference_with_fallback(
    registry: &ModelProviderRegistry,
    model_group: &ModelGroup,
    system_prompt: &str,
    history: Vec<RigMessage>,
    user_message: RigMessage,
) -> Result<String, LlmError> {
    let mut errors = Vec::new();
    let max_tokens = model_group.max_tokens;
    let temperature = model_group.temperature;
    let max_output = max_tokens.unwrap_or(8192) as usize;

    let truncated = truncate_history(
        history,
        system_prompt,
        &model_group.main.model_id,
        model_group.context_window,
        max_output,
    );

    let ref_str = model_group.main.as_str();
    match retry_inference(registry, &model_group.main, system_prompt, &truncated, &user_message, max_tokens, temperature, &model_group.retry).await {
        Ok(response) => {
            tracing::info!(model = %ref_str, "Completion succeeded");
            return Ok(response);
        }
        Err(e) => {
            tracing::warn!(model = %ref_str, error = %e, "Main model failed, trying fallbacks");
            errors.push((ref_str, e.to_string()));
        }
    }

    for fallback in &model_group.fallbacks {
        let ref_str = fallback.as_str();
        let truncated_fb = truncate_history(
            truncated.clone(),
            system_prompt,
            &fallback.model_id,
            model_group.context_window,
            max_output,
        );
        match retry_inference(registry, fallback, system_prompt, &truncated_fb, &user_message, max_tokens, temperature, &model_group.retry).await {
            Ok(response) => {
                tracing::info!(model = %ref_str, "Fallback succeeded");
                return Ok(response);
            }
            Err(e) => {
                tracing::warn!(model = %ref_str, error = %e, "Fallback failed");
                errors.push((ref_str, e.to_string()));
            }
        }
    }

    Err(LlmError::AllFallbacksFailed(errors))
}

pub async fn stream_inference_with_fallback(
    registry: &ModelProviderRegistry,
    model_group: &ModelGroup,
    system_prompt: &str,
    history: Vec<RigMessage>,
    user_message: RigMessage,
    token_tx: mpsc::Sender<Result<String, LlmError>>,
) -> Result<(), LlmError> {
    let mut errors = Vec::new();
    let max_tokens = model_group.max_tokens;
    let temperature = model_group.temperature;
    let max_output = max_tokens.unwrap_or(8192) as usize;

    let truncated = truncate_history(
        history,
        system_prompt,
        &model_group.main.model_id,
        model_group.context_window,
        max_output,
    );

    let ref_str = model_group.main.as_str();
    match retry_stream_inference(
        registry,
        &model_group.main,
        system_prompt,
        &truncated,
        &user_message,
        token_tx.clone(),
        max_tokens,
        temperature,
        &model_group.retry,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(model = %ref_str, "Stream succeeded");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(model = %ref_str, error = %e, "Main model stream failed, trying fallbacks");
            errors.push((ref_str, e.to_string()));
        }
    }

    for fallback in &model_group.fallbacks {
        let ref_str = fallback.as_str();
        let truncated_fb = truncate_history(
            truncated.clone(),
            system_prompt,
            &fallback.model_id,
            model_group.context_window,
            max_output,
        );
        match retry_stream_inference(
            registry,
            fallback,
            system_prompt,
            &truncated_fb,
            &user_message,
            token_tx.clone(),
            max_tokens,
            temperature,
            &model_group.retry,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(model = %ref_str, "Fallback stream succeeded");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(model = %ref_str, error = %e, "Fallback stream failed");
                errors.push((ref_str, e.to_string()));
            }
        }
    }

    Err(LlmError::AllFallbacksFailed(errors))
}

async fn retry_inference(
    registry: &ModelProviderRegistry,
    model_ref: &super::provider::ModelRef,
    system_prompt: &str,
    history: &[RigMessage],
    user_message: &RigMessage,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    retry_config: &RetryConfig,
) -> Result<String, LlmError> {
    let model_str = model_ref.as_str().to_string();
    (|| async {
        try_inference(registry, model_ref, system_prompt, history, user_message, max_tokens, temperature).await
    })
    .retry(retry_config.to_backoff())
    .sleep(tokio::time::sleep)
    .when(|e| e.is_retryable())
    .notify(|e, dur| {
        tracing::warn!(model = %model_str, error = %e, delay = ?dur, "Retryable error, backing off");
    })
    .await
}

async fn retry_stream_inference(
    registry: &ModelProviderRegistry,
    model_ref: &super::provider::ModelRef,
    system_prompt: &str,
    history: &[RigMessage],
    user_message: &RigMessage,
    token_tx: mpsc::Sender<Result<String, LlmError>>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    retry_config: &RetryConfig,
) -> Result<(), LlmError> {
    let model_str = model_ref.as_str().to_string();
    (|| async {
        try_stream_inference(registry, model_ref, system_prompt, history, user_message, token_tx.clone(), max_tokens, temperature).await
    })
    .retry(retry_config.to_backoff())
    .sleep(tokio::time::sleep)
    .when(|e| e.is_retryable())
    .notify(|e, dur| {
        tracing::warn!(model = %model_str, error = %e, delay = ?dur, "Retryable stream error, backing off");
    })
    .await
}

async fn try_inference(
    registry: &ModelProviderRegistry,
    model_ref: &super::provider::ModelRef,
    system_prompt: &str,
    history: &[RigMessage],
    user_message: &RigMessage,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> Result<String, LlmError> {
    let provider = registry.get_provider(&model_ref.provider)?;
    provider
        .inference(
            &model_ref.model_id,
            system_prompt,
            history.to_vec(),
            user_message.clone(),
            max_tokens,
            temperature,
        )
        .await
}

async fn try_stream_inference(
    registry: &ModelProviderRegistry,
    model_ref: &super::provider::ModelRef,
    system_prompt: &str,
    history: &[RigMessage],
    user_message: &RigMessage,
    token_tx: mpsc::Sender<Result<String, LlmError>>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> Result<(), LlmError> {
    let provider = registry.get_provider(&model_ref.provider)?;
    provider
        .stream_inference(
            &model_ref.model_id,
            system_prompt,
            history.to_vec(),
            user_message.clone(),
            token_tx,
            max_tokens,
            temperature,
        )
        .await
}
