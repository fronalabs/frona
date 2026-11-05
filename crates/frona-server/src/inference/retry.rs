use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use backon::Retryable;
use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};

use crate::inference::context::{estimate_message_tokens, estimate_tokens, truncate_history};
use crate::inference::error::InferenceError;
use crate::inference::observation::{AttemptOutcome, LatencyMetrics};
use crate::inference::provider::ModelProvider;
use crate::inference::provider::{InferenceOutput, ModelConfig};
use crate::inference::{ModelGroup, ModelRequest, config::RetryConfig};
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

/// How a completion handles input that exceeds its configured context budget.
#[derive(Clone, Copy)]
pub(crate) enum HistoryOverflow {
    TruncateOldest,
    Reject,
}

pub(crate) fn completion_history(
    history: &[RigMessage],
    system_prompt: &str,
    tools: &[RigToolDefinition],
    group: &ModelGroup,
    max_output: usize,
    overflow: HistoryOverflow,
    input_token_estimate: Option<crate::inference::context::InputTokenEstimate>,
) -> Result<Vec<RigMessage>, String> {
    if let Some(tokens) = input_token_estimate
        .and_then(|estimate| estimate.for_request(&group.main, system_prompt, history, tools))
    {
        let available = group.context_window.saturating_sub(max_output);
        if tokens > available {
            return Err(format!(
                "Complete input exceeds context limit ({tokens} calibrated tokens, {available} available)"
            ));
        }
        return Ok(history.to_vec());
    }
    if matches!(overflow, HistoryOverflow::TruncateOldest) {
        return truncate_history(
            history.to_vec(),
            system_prompt,
            group.context_window,
            max_output,
            group.inference.history_truncation_pct,
        );
    }
    let tokens = history.iter().map(estimate_message_tokens).sum::<usize>()
        + estimate_tokens(system_prompt)
        + tools
            .iter()
            .map(|tool| {
                estimate_tokens(
                    &serde_json::to_string(tool).expect("tool definition is serializable"),
                )
            })
            .sum::<usize>();
    let available = group.context_window.saturating_sub(max_output);
    if tokens > available {
        return Err(format!(
            "Complete input exceeds context limit ({tokens} estimated tokens, {available} available)"
        ));
    }
    Ok(history.to_vec())
}

pub(crate) async fn inference_with_retry_and_fallback(
    model_group: &ModelGroup,
    request: ModelRequest<'_>,
    overflow: HistoryOverflow,
) -> Result<(Vec<AssistantContent>, crate::inference::Usage), InferenceError> {
    let ModelRequest {
        overrides,
        input_token_estimate,
        system_prompt,
        history,
        tools,
        observations,
    } = request;
    ensure_usable(&model_group.providers, model_group).await?;
    let mut errors = Vec::new();
    let max_tokens = overrides
        .max_tokens
        .or(model_group.main.request_settings.max_tokens);
    let temperature = overrides
        .temperature
        .or(model_group.main.request_settings.temperature);
    let max_output = max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize;

    let candidate_history = completion_history(
        &history,
        system_prompt,
        &tools,
        model_group,
        max_output,
        overflow,
        input_token_estimate,
    );

    let ref_str = model_group.main.as_str();
    let start = Instant::now();
    match retry_with_backoff(&model_group.retry, &model_group.main, || async {
        let request_history = candidate_history
            .as_ref()
            .map_err(|error| InferenceError::InferenceFailed(error.clone()))?;
        let provider = provider_for(&model_group.providers, &model_group.main)?;
        let attempt_start = Instant::now();
        let result = provider
            .inference(
                &model_group.main,
                system_prompt,
                request_history.clone(),
                tools.clone(),
                max_tokens,
                temperature,
            )
            .await;
        observations
            .attempt(
                &model_group.main,
                AttemptOutcome::from_result(&result),
                result.as_ref().ok().map(|output| &output.usage),
                attempt_start.elapsed().as_millis() as u64,
            )
            .await;
        result
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
            observations
                .usage(&model_group.main, Some(&usage), 0, latency)
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
        let candidate_history = completion_history(
            &history,
            system_prompt,
            &tools,
            model_group,
            max_output,
            overflow,
            None,
        );
        let start = Instant::now();
        match retry_with_backoff(&model_group.retry, fallback, || async {
            let request_history = candidate_history
                .as_ref()
                .map_err(|error| InferenceError::InferenceFailed(error.clone()))?;
            let provider = provider_for(&model_group.providers, fallback)?;
            let attempt_start = Instant::now();
            let result = provider
                .inference(
                    fallback,
                    system_prompt,
                    request_history.clone(),
                    tools.clone(),
                    max_tokens,
                    temperature,
                )
                .await;
            observations
                .attempt(
                    fallback,
                    AttemptOutcome::from_result(&result),
                    result.as_ref().ok().map(|output| &output.usage),
                    attempt_start.elapsed().as_millis() as u64,
                )
                .await;
            result
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
                observations
                    .usage(fallback, Some(&usage), (idx + 1) as u8, latency)
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
    request: ModelRequest<'_>,
    schema: serde_json::Value,
) -> Result<serde_json::Value, InferenceError> {
    let ModelRequest {
        overrides,
        system_prompt,
        history,
        observations,
        ..
    } = request;
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
        let request_history = truncated
            .as_ref()
            .map_err(|error| InferenceError::InferenceFailed(error.clone()))?;
        let provider = provider_for(&model_group.providers, &model_group.main)?;
        let attempt_start = Instant::now();
        let result = provider
            .structured_inference(
                &model_group.main,
                system_prompt,
                request_history.clone(),
                schema.clone(),
                max_tokens,
                temperature,
            )
            .await;
        observations
            .attempt(
                &model_group.main,
                AttemptOutcome::from_result(&result),
                None,
                attempt_start.elapsed().as_millis() as u64,
            )
            .await;
        result
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
            // Structured providers do not expose usage on this path. Preserve
            // that absence for observers; the persistent adapter keeps its
            // existing row representation. No token estimate is invented.
            let latency = LatencyMetrics {
                duration_ms,
                ttft_ms: None,
                retry_overhead_ms,
                retry_count,
            };
            observations
                .usage(&model_group.main, None, 0, latency)
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
            let request_history = truncated_fb
                .as_ref()
                .map_err(|error| InferenceError::InferenceFailed(error.clone()))?;
            let provider = provider_for(&model_group.providers, fallback)?;
            let attempt_start = Instant::now();
            let result = provider
                .structured_inference(
                    fallback,
                    system_prompt,
                    request_history.clone(),
                    schema.clone(),
                    max_tokens,
                    temperature,
                )
                .await;
            observations
                .attempt(
                    fallback,
                    AttemptOutcome::from_result(&result),
                    None,
                    attempt_start.elapsed().as_millis() as u64,
                )
                .await;
            result
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
                observations
                    .usage(fallback, None, (idx + 1) as u8, latency)
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

pub(crate) fn provider_for(
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
