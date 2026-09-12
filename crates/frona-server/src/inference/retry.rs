use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use backon::Retryable;
use rig_core::completion::message::UserContent;
use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};
use tokio::sync::mpsc;

use crate::chat::broadcast::EventSender;

use crate::inference::context::truncate_history;
use crate::inference::error::InferenceError;
use crate::inference::provider::ModelProvider;
use crate::inference::provider::{InferenceOutput, ModelConfig, StreamToken};
use crate::inference::tool_loop::{InferenceEvent, InferenceEventKind};
use crate::inference::usage::UsageContext;
use crate::inference::usage::{LatencyMetrics, UsageService};
use crate::inference::{ModelGroup, RequestOverrides, config::RetryConfig};
use std::collections::HashMap;

/// Result of a retried operation with retry instrumentation. `retry_count` is
/// the number of failed attempts before the success; `retry_overhead_ms` is
/// the wall time from the outer start to the start of the successful attempt
/// (i.e. time spent in failed attempts + backoff sleeps).
pub struct RetryOutcome<T> {
    pub value: T,
    pub retry_count: u32,
    pub retry_overhead_ms: u64,
}

pub async fn retry_with_backoff<T, F, Fut>(
    retry_config: &RetryConfig,
    model_ref: &ModelConfig,
    op: F,
) -> Result<RetryOutcome<T>, InferenceError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, InferenceError>>,
{
    let model_str = model_ref.as_str();
    let outer_start = Instant::now();
    // ms-from-outer-start at which the LAST attempt began. After backon
    // returns Ok, this is the start of the successful attempt - so it equals
    // the time spent in failed attempts + backoff sleeps before success.
    let last_attempt_start_ms = Arc::new(AtomicU64::new(0));
    let retries = Arc::new(AtomicU32::new(0));

    let lams_op = last_attempt_start_ms.clone();
    let lams_notify = last_attempt_start_ms.clone();
    let retries_notify = retries.clone();

    let value = (|| {
        let lams = lams_op.clone();
        let fut = op();
        async move {
            lams.store(outer_start.elapsed().as_millis() as u64, Ordering::Relaxed);
            fut.await
        }
    })
    .retry(retry_config.to_backoff())
    .sleep(tokio::time::sleep)
    .when(|e| e.is_retryable())
    .notify(move |e, dur| {
        retries_notify.fetch_add(1, Ordering::Relaxed);
        // Defensive: zero out so `last_attempt_start_ms` is set fresh by the
        // next op() invocation. (Strictly redundant - op() always overwrites
        // before any read - but keeps the invariant local to this helper.)
        lams_notify.store(0, Ordering::Relaxed);
        tracing::warn!(model = %model_str, error = %e, delay = ?dur, "Retryable error, backing off");
    })
    .await
    .map_err(|error| error.for_model(model_ref, retries.load(Ordering::Relaxed)))?;

    Ok(RetryOutcome {
        value,
        retry_count: retries.load(Ordering::Relaxed),
        retry_overhead_ms: last_attempt_start_ms.load(Ordering::Relaxed),
    })
}

pub(crate) async fn inference_with_retry_and_fallback(
    model_group: &ModelGroup,
    overrides: RequestOverrides,
    system_prompt: &str,
    history: Vec<RigMessage>,
    tools: Vec<RigToolDefinition>,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
) -> Result<(Vec<AssistantContent>, crate::inference::Usage), InferenceError> {
    ensure_usable(&model_group.providers, model_group).await?;
    let mut errors = Vec::new();
    let max_tokens = overrides
        .max_tokens
        .or(model_group.main.request_settings.max_tokens);
    let temperature = overrides
        .temperature
        .or(model_group.main.request_settings.temperature);
    let max_output = max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize;
    let truncation_pct = model_group.inference.history_truncation_pct;

    let truncated = truncate_history(
        history.clone(),
        system_prompt,
        model_group.context_window,
        max_output,
        truncation_pct,
    );

    let ref_str = model_group.main.as_str();
    let start = Instant::now();
    match retry_with_backoff(&model_group.retry, &model_group.main, || async {
        let provider = provider_for(&model_group.providers, &model_group.main)?;
        provider
            .inference(
                &model_group.main,
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
        Ok(RetryOutcome {
            value:
                InferenceOutput {
                    content,
                    usage,
                    ttft_ms,
                },
            retry_count,
            retry_overhead_ms,
        }) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            tracing::debug!(model = %ref_str, "Completion succeeded");
            let latency = LatencyMetrics {
                duration_ms,
                ttft_ms,
                retry_overhead_ms,
                retry_count,
            };
            usage_service
                .record(usage_ctx, &model_group.main, &usage, 0, latency)
                .await;
            return Ok((content, usage));
        }
        Err(e) => {
            tracing::warn!(model = %ref_str, error = %e, "Main model failed, trying fallbacks");
            errors.push(e);
        }
    }

    for (idx, fallback) in model_group.fallbacks.iter().enumerate() {
        let max_tokens = overrides
            .max_tokens
            .or(fallback.request_settings.max_tokens);
        let temperature = overrides
            .temperature
            .or(fallback.request_settings.temperature);
        let max_output = max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize;
        let ref_str = fallback.as_str();
        let truncated_fb = truncate_history(
            history.clone(),
            system_prompt,
            model_group.context_window,
            max_output,
            truncation_pct,
        );
        let start = Instant::now();
        match retry_with_backoff(&model_group.retry, fallback, || async {
            let provider = provider_for(&model_group.providers, fallback)?;
            provider
                .inference(
                    fallback,
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
            Ok(RetryOutcome {
                value:
                    InferenceOutput {
                        content,
                        usage,
                        ttft_ms,
                    },
                retry_count,
                retry_overhead_ms,
            }) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                tracing::info!(model = %ref_str, "Fallback succeeded");
                let latency = LatencyMetrics {
                    duration_ms,
                    ttft_ms,
                    retry_overhead_ms,
                    retry_count,
                };
                usage_service
                    .record(usage_ctx, fallback, &usage, (idx + 1) as u8, latency)
                    .await;
                return Ok((content, usage));
            }
            Err(e) => {
                tracing::warn!(model = %ref_str, error = %e, "Fallback failed");
                errors.push(e);
            }
        }
    }

    Err(InferenceError::AllFallbacksFailed(errors))
}

pub(crate) async fn structured_inference_with_retry_and_fallback(
    model_group: &ModelGroup,
    overrides: RequestOverrides,
    system_prompt: &str,
    history: Vec<RigMessage>,
    schema: serde_json::Value,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
) -> Result<serde_json::Value, InferenceError> {
    ensure_usable(&model_group.providers, model_group).await?;
    let mut errors = Vec::new();
    let max_tokens = overrides
        .max_tokens
        .or(model_group.main.request_settings.max_tokens);
    let temperature = overrides
        .temperature
        .or(model_group.main.request_settings.temperature);
    let max_output = max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize;
    let truncation_pct = model_group.inference.history_truncation_pct;

    let truncated = truncate_history(
        history.clone(),
        system_prompt,
        model_group.context_window,
        max_output,
        truncation_pct,
    );

    let ref_str = model_group.main.as_str();
    let start = Instant::now();
    match retry_with_backoff(&model_group.retry, &model_group.main, || async {
        let provider = provider_for(&model_group.providers, &model_group.main)?;
        provider
            .structured_inference(
                &model_group.main,
                system_prompt,
                truncated.clone(),
                schema.clone(),
                max_tokens,
                temperature,
            )
            .await
    })
    .await
    {
        Ok(RetryOutcome {
            value,
            retry_count,
            retry_overhead_ms,
        }) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            tracing::info!(model = %ref_str, "Structured extraction succeeded");
            // structured_inference at the rig layer doesn't surface a Usage -
            // we record the call with zeros so the row + Prom counter still
            // captures cost-irrelevant volume. ttft_ms is None: non-streaming.
            let latency = LatencyMetrics {
                duration_ms,
                ttft_ms: None,
                retry_overhead_ms,
                retry_count,
            };
            usage_service
                .record(
                    usage_ctx,
                    &model_group.main,
                    &crate::inference::Usage::default(),
                    0,
                    latency,
                )
                .await;
            return Ok(value);
        }
        Err(e) => {
            tracing::warn!(model = %ref_str, error = %e, "Structured extraction failed on main, trying fallbacks");
            errors.push(e);
        }
    }

    for (idx, fallback) in model_group.fallbacks.iter().enumerate() {
        let max_tokens = overrides
            .max_tokens
            .or(fallback.request_settings.max_tokens);
        let temperature = overrides
            .temperature
            .or(fallback.request_settings.temperature);
        let max_output = max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize;
        let ref_str = fallback.as_str();
        let truncated_fb = truncate_history(
            history.clone(),
            system_prompt,
            model_group.context_window,
            max_output,
            truncation_pct,
        );
        let start = Instant::now();
        match retry_with_backoff(&model_group.retry, fallback, || async {
            let provider = provider_for(&model_group.providers, fallback)?;
            provider
                .structured_inference(
                    fallback,
                    system_prompt,
                    truncated_fb.clone(),
                    schema.clone(),
                    max_tokens,
                    temperature,
                )
                .await
        })
        .await
        {
            Ok(RetryOutcome {
                value,
                retry_count,
                retry_overhead_ms,
            }) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                tracing::info!(model = %ref_str, "Structured extraction fallback succeeded");
                let latency = LatencyMetrics {
                    duration_ms,
                    ttft_ms: None,
                    retry_overhead_ms,
                    retry_count,
                };
                usage_service
                    .record(
                        usage_ctx,
                        fallback,
                        &crate::inference::Usage::default(),
                        (idx + 1) as u8,
                        latency,
                    )
                    .await;
                return Ok(value);
            }
            Err(e) => {
                tracing::warn!(model = %ref_str, error = %e, "Structured extraction fallback failed");
                errors.push(e);
            }
        }
    }

    Err(InferenceError::AllFallbacksFailed(errors))
}

pub enum StreamResult {
    Contents {
        content: Vec<AssistantContent>,
        usage: crate::inference::Usage,
    },
    Cancelled,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry_and_fallback(
    model_group: &ModelGroup,
    overrides: RequestOverrides,
    system_prompt: &str,
    chat_history: &[RigMessage],
    tools: &[RigToolDefinition],
    event_tx: &EventSender,
    cancel_token: &tokio_util::sync::CancellationToken,
    accumulated_text: &mut String,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
) -> Result<StreamResult, crate::core::error::AppError> {
    let mut failures = Vec::new();
    for (index, model) in std::iter::once(&model_group.main)
        .chain(&model_group.fallbacks)
        .enumerate()
    {
        let provider = match provider_for(&model_group.providers, model) {
            Ok(provider) => provider,
            Err(error) => {
                failures.push(error.for_model(model, 0));
                continue;
            }
        };
        let max_tokens = overrides.max_tokens.or(model.request_settings.max_tokens);
        let temperature = overrides.temperature.or(model.request_settings.temperature);
        let history = truncate_history(
            chat_history.to_vec(),
            system_prompt,
            model_group.context_window,
            max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize,
            model_group.inference.history_truncation_pct,
        );
        let emitted = Arc::new(AtomicBool::new(false));
        match stream_one_model(
            provider,
            emitted.clone(),
            model_group,
            model,
            RequestOverrides {
                max_tokens,
                temperature,
            },
            index as u8,
            system_prompt,
            &history,
            tools,
            event_tx,
            cancel_token,
            accumulated_text,
            usage_service,
            usage_ctx,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                failures.push(error);
                if emitted.load(Ordering::Relaxed) {
                    return Err(InferenceError::AllFallbacksFailed(failures).into());
                }
            }
        }
    }
    Err(InferenceError::AllFallbacksFailed(failures).into())
}

#[allow(clippy::too_many_arguments)]
async fn stream_one_model(
    provider: Arc<dyn crate::inference::provider::ModelProvider>,
    emitted: Arc<AtomicBool>,
    model_group: &ModelGroup,
    model_config: &ModelConfig,
    overrides: RequestOverrides,
    fallback_index: u8,
    system_prompt: &str,
    chat_history: &[RigMessage],
    tools: &[RigToolDefinition],
    event_tx: &EventSender,
    cancel_token: &tokio_util::sync::CancellationToken,
    accumulated_text: &mut String,
    usage_service: &UsageService,
    usage_ctx: &UsageContext,
) -> Result<StreamResult, InferenceError> {
    let model_str = model_config.as_str();

    // Track retry stats across the inline backon loop. Same idea as
    // `retry_with_backoff`: count notify-fires for retries; track the start
    // offset of the last attempt for overhead.
    let outer_start = Instant::now();
    let last_attempt_start_ms = Arc::new(AtomicU64::new(0));
    let retries = Arc::new(AtomicU32::new(0));
    let lams_notify = last_attempt_start_ms.clone();
    let retries_notify = retries.clone();

    let result = (|| async {
        last_attempt_start_ms.store(outer_start.elapsed().as_millis() as u64, Ordering::Relaxed);
        let (text_tx, text_rx) = mpsc::channel::<StreamToken>(64);

        let event_tx_clone = event_tx.clone();
        let emitted = emitted.clone();
        let forward_handle = tokio::spawn(async move {
            let mut text_rx = text_rx;
            let mut text = String::new();
            while let Some(token) = text_rx.recv().await {
                emitted.store(true, Ordering::Relaxed);
                match token {
                    StreamToken::Text(t) => {
                        text.push_str(&t);
                        event_tx_clone.send(InferenceEvent {
                            kind: InferenceEventKind::Text(t),
                        });
                    }
                    StreamToken::Reasoning(r) => {
                        event_tx_clone.send(InferenceEvent {
                            kind: InferenceEventKind::Reasoning(r),
                        });
                    }
                }
            }
            text
        });

        let attempt_start = Instant::now();
        let contents_result = tokio::select! {
            result = provider.stream_inference(
                model_config,
                system_prompt,
                chat_history.to_vec(),
                tools.to_vec(),
                text_tx,
                overrides.max_tokens,
                overrides.temperature,
            ) => Some(result),
            _ = cancel_token.cancelled() => None,
        };
        let duration_ms = attempt_start.elapsed().as_millis() as u64;

        let turn_text = forward_handle.await.unwrap_or_default();

        match contents_result {
            None => Err(InferenceError::Cancelled(turn_text)),
            Some(Ok(output)) if output.content.is_empty() => {
                let last_is_tool_result = matches!(
                    chat_history.last(),
                    Some(RigMessage::User { content }) if content.iter().any(|c| matches!(c, UserContent::ToolResult(_)))
                );
                let last_is_assistant = matches!(
                    chat_history.last(),
                    Some(RigMessage::Assistant { .. })
                );
                if last_is_tool_result || last_is_assistant {
                    let retry_overhead_ms = outer_start.elapsed().as_millis() as u64 - duration_ms;
                    let latency = LatencyMetrics {
                        duration_ms,
                        ttft_ms: output.ttft_ms,
                        retry_overhead_ms,
                        retry_count: retries.load(Ordering::Relaxed),
                    };
                    usage_service
                        .record(usage_ctx, model_config, &output.usage, fallback_index, latency)
                        .await;
                    Ok((output, turn_text))
                } else {
                    Err(InferenceError::EmptyResponse)
                }
            }
            Some(Ok(output)) => {
                let retry_overhead_ms = outer_start.elapsed().as_millis() as u64 - duration_ms;
                let latency = LatencyMetrics {
                    duration_ms,
                    ttft_ms: output.ttft_ms,
                    retry_overhead_ms,
                    retry_count: retries.load(Ordering::Relaxed),
                };
                usage_service
                    .record(usage_ctx, model_config, &output.usage, fallback_index, latency)
                    .await;
                Ok((output, turn_text))
            }
            Some(Err(e)) => Err(e),
        }
    })
    .retry(model_group.retry.to_backoff())
    .sleep(tokio::time::sleep)
    .when(|e| e.is_retryable() && !emitted.load(Ordering::Relaxed))
    .notify(|e, dur| {
        retries_notify.fetch_add(1, Ordering::Relaxed);
        lams_notify.store(0, Ordering::Relaxed);
        tracing::warn!(model = %model_str, error = %e, delay = ?dur, "Retryable error, backing off");
        event_tx.send(InferenceEvent {
            kind: InferenceEventKind::Retry {
                retry_after_ms: dur.as_millis() as u64,
                reason: e.retry_reason(),
            },
        });
    })
    .await;

    match result {
        Ok((InferenceOutput { content, usage, .. }, text)) => {
            accumulated_text.push_str(&text);
            Ok(StreamResult::Contents { content, usage })
        }
        Err(InferenceError::Cancelled(text)) => {
            accumulated_text.push_str(&text);
            Ok(StreamResult::Cancelled)
        }
        Err(e) => Err(e.for_model(model_config, retries.load(Ordering::Relaxed))),
    }
}

fn provider_for(
    providers: &HashMap<String, Arc<dyn ModelProvider>>,
    model: &ModelConfig,
) -> Result<Arc<dyn ModelProvider>, InferenceError> {
    providers
        .get(model.provider_name())
        .cloned()
        .ok_or_else(|| InferenceError::ProviderNotConfigured(model.provider_name().into()))
}

async fn ensure_usable(
    providers: &HashMap<String, Arc<dyn ModelProvider>>,
    group: &ModelGroup,
) -> Result<(), InferenceError> {
    let mut reasons = Vec::new();
    for model in std::iter::once(&group.main).chain(&group.fallbacks) {
        let result = match provider_for(providers, model) {
            Ok(provider) => provider.ensure_usable(model).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) => reasons.push(error.for_model(model, 0)),
        }
    }
    Err(InferenceError::AllFallbacksFailed(reasons))
}
