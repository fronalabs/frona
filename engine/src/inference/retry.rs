use std::future::Future;
use std::time::Instant;

use backon::Retryable;
use rig::completion::request::ToolDefinition as RigToolDefinition;
use rig::completion::{AssistantContent, Message as RigMessage};
use tokio::sync::mpsc;

use crate::core::metrics::{self, InferenceMetricsContext};

use super::config::{ModelGroup, RetryConfig};
use super::context::truncate_history;
use super::error::InferenceError;
use super::provider::ModelRef;
use super::registry::ModelProviderRegistry;
use super::tool_loop::{InferenceEvent, InferenceEventKind};

pub async fn retry_with_backoff<T, F, Fut>(
    retry_config: &RetryConfig,
    model_ref: &ModelRef,
    op: F,
) -> Result<T, InferenceError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, InferenceError>>,
{
    let model_str = model_ref.as_str();
    (|| async { op().await })
        .retry(retry_config.to_backoff())
        .sleep(tokio::time::sleep)
        .when(|e| e.is_retryable())
        .notify(|e, dur| {
            tracing::warn!(model = %model_str, error = %e, delay = ?dur, "Retryable error, backing off");
        })
        .await
}

pub async fn inference_with_retry_and_fallback(
    registry: &ModelProviderRegistry,
    model_group: &ModelGroup,
    system_prompt: &str,
    history: Vec<RigMessage>,
    tools: Vec<RigToolDefinition>,
    metrics_ctx: &InferenceMetricsContext,
) -> Result<(Vec<AssistantContent>, crate::inference::Usage), InferenceError> {
    let mut errors = Vec::new();
    let max_tokens = model_group.max_tokens;
    let temperature = model_group.temperature;
    let max_output = max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize;
    let truncation_pct = model_group.inference.history_truncation_pct;

    let truncated = truncate_history(
        history,
        system_prompt,
        &model_group.main.model_id,
        model_group.context_window,
        max_output,
        truncation_pct,
    );

    let ref_str = model_group.main.as_str();
    let start = Instant::now();
    match retry_with_backoff(&model_group.retry, &model_group.main, || async {
        let provider = registry.get_provider(&model_group.main.provider)?;
        provider
            .inference(
                &model_group.main.model_id,
                system_prompt,
                truncated.clone(),
                tools.clone(),
                max_tokens,
                temperature,
            )
            .await
    })
    .await
    {
        Ok((contents, usage)) => {
            let duration = start.elapsed();
            tracing::info!(model = %ref_str, "Completion succeeded");
            metrics::record_inference_request(
                metrics_ctx,
                &model_group.main.model_id,
                &model_group.main.provider,
                duration,
                Some(&usage),
                "success",
            );
            return Ok((contents, usage));
        }
        Err(e) => {
            let duration = start.elapsed();
            tracing::warn!(model = %ref_str, error = %e, "Main model failed, trying fallbacks");
            metrics::record_inference_request(
                metrics_ctx,
                &model_group.main.model_id,
                &model_group.main.provider,
                duration,
                None,
                "error",
            );
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
            truncation_pct,
        );
        let start = Instant::now();
        match retry_with_backoff(&model_group.retry, fallback, || async {
            let provider = registry.get_provider(&fallback.provider)?;
            provider
                .inference(
                    &fallback.model_id,
                    system_prompt,
                    truncated_fb.clone(),
                    tools.clone(),
                    max_tokens,
                    temperature,
                )
                .await
        })
        .await
        {
            Ok((contents, usage)) => {
                let duration = start.elapsed();
                tracing::info!(model = %ref_str, "Fallback succeeded");
                metrics::record_inference_request(
                    metrics_ctx,
                    &fallback.model_id,
                    &fallback.provider,
                    duration,
                    Some(&usage),
                    "success",
                );
                return Ok((contents, usage));
            }
            Err(e) => {
                let duration = start.elapsed();
                tracing::warn!(model = %ref_str, error = %e, "Fallback failed");
                metrics::record_inference_request(
                    metrics_ctx,
                    &fallback.model_id,
                    &fallback.provider,
                    duration,
                    None,
                    "error",
                );
                errors.push((ref_str, e.to_string()));
            }
        }
    }

    Err(InferenceError::AllFallbacksFailed(errors))
}

pub enum StreamResult {
    Contents(Vec<AssistantContent>),
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
pub async fn stream_with_retry_and_fallback(
    registry: &ModelProviderRegistry,
    model_group: &ModelGroup,
    system_prompt: &str,
    chat_history: &[RigMessage],
    tools: &[RigToolDefinition],
    event_tx: &mpsc::UnboundedSender<InferenceEvent>,
    cancel_token: &tokio_util::sync::CancellationToken,
    accumulated_text: &mut String,
    metrics_ctx: &InferenceMetricsContext,
) -> Result<StreamResult, crate::core::error::AppError> {
    let provider = registry
        .get_provider(&model_group.main.provider)
        .map_err(|e| crate::core::error::AppError::Inference(e.to_string()))?;

    let model_id = &model_group.main.model_id;
    let provider_name = &model_group.main.provider;
    let model_str = model_group.main.as_str();

    let result = (|| async {
        let (text_tx, text_rx) = mpsc::channel::<String>(64);

        let event_tx_clone = event_tx.clone();
        let forward_handle = tokio::spawn(async move {
            let mut text_rx = text_rx;
            let mut text = String::new();
            while let Some(token) = text_rx.recv().await {
                text.push_str(&token);
                let _ = event_tx_clone
                    .send(InferenceEvent {
                        kind: InferenceEventKind::Text(token),
                    });
            }
            text
        });

        let start = Instant::now();
        let contents_result = tokio::select! {
            result = provider.stream_inference(
                model_id,
                system_prompt,
                chat_history.to_vec(),
                tools.to_vec(),
                text_tx,
                model_group.max_tokens,
                model_group.temperature,
            ) => Some(result),
            _ = cancel_token.cancelled() => None,
        };
        let duration = start.elapsed();

        let turn_text = forward_handle.await.unwrap_or_default();

        match contents_result {
            None => Err(InferenceError::Cancelled(turn_text)),
            Some(Ok(contents)) if contents.is_empty() => {
                metrics::record_inference_request(
                    metrics_ctx, model_id, provider_name, duration, None, "empty_response",
                );
                Err(InferenceError::EmptyResponse)
            }
            Some(Ok(contents)) => {
                metrics::record_inference_request(
                    metrics_ctx, model_id, provider_name, duration, None, "success",
                );
                Ok((contents, turn_text))
            }
            Some(Err(e)) => {
                let status = if e.is_rate_limited() {
                    "rate_limited"
                } else {
                    "error"
                };
                metrics::record_inference_request(
                    metrics_ctx, model_id, provider_name, duration, None, status,
                );
                Err(e)
            }
        }
    })
    .retry(model_group.retry.to_backoff())
    .sleep(tokio::time::sleep)
    .when(|e| e.is_retryable())
    .notify(|e, dur| {
        tracing::warn!(model = %model_str, error = %e, delay = ?dur, "Retryable error, backing off");
        if e.is_rate_limited() {
            let _ = event_tx.send(InferenceEvent {
                kind: InferenceEventKind::RateLimitRetry {
                    retry_after_ms: dur.as_millis() as u64,
                },
            });
        }
    })
    .await;

    match result {
        Ok((contents, text)) => {
            accumulated_text.push_str(&text);
            Ok(StreamResult::Contents(contents))
        }
        Err(InferenceError::Cancelled(text)) => {
            accumulated_text.push_str(&text);
            Ok(StreamResult::Cancelled)
        }
        Err(e) => Err(crate::core::error::AppError::from(e)),
    }
}

