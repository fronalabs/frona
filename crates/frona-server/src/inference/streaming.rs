//! Server event transport around portable model execution.
use crate::{
    chat::broadcast::EventSender,
    core::error::AppError,
    inference::tool_loop::{InferenceEvent, InferenceEventKind},
};
use backon::Retryable;
use frona_harness::inference::{
    InferenceError, ModelGroup, ModelRequest, RequestOverrides,
    observation::{AttemptOutcome, LatencyMetrics, Observations},
    provider::{InferenceOutput, ModelConfig, StreamToken},
};
use rig_core::completion::{
    AssistantContent, Message as RigMessage, message::UserContent,
    request::ToolDefinition as RigToolDefinition,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc;

/// Application streaming translates provider tokens into Frona events. Keeping
/// this local trait separate permits ModelGroup to live in a portable crate.
#[async_trait::async_trait]
pub trait ModelGroupStreaming {
    async fn stream_inference(
        &self,
        request: ModelRequest<'_>,
        events: &EventSender,
        cancellation: &tokio_util::sync::CancellationToken,
        accumulated_text: &mut String,
    ) -> Result<StreamResult, AppError>;
}
#[async_trait::async_trait]
impl ModelGroupStreaming for ModelGroup {
    async fn stream_inference(
        &self,
        request: ModelRequest<'_>,
        events: &EventSender,
        cancellation: &tokio_util::sync::CancellationToken,
        accumulated_text: &mut String,
    ) -> Result<StreamResult, AppError> {
        stream_with_retry_and_fallback(self, request, events, cancellation, accumulated_text).await
    }
}

pub enum StreamResult {
    Contents {
        content: Vec<AssistantContent>,
        usage: frona_harness::inference::Usage,
    },
    Cancelled,
}

pub(crate) async fn stream_with_retry_and_fallback(
    model_group: &ModelGroup,
    request: ModelRequest<'_>,
    event_tx: &EventSender,
    cancel_token: &tokio_util::sync::CancellationToken,
    accumulated_text: &mut String,
) -> Result<StreamResult, crate::core::error::AppError> {
    let ModelRequest {
        overrides,
        input_token_estimate,
        system_prompt,
        history,
        tools,
        observations,
    } = request;
    let mut failures = Vec::new();
    for (index, model) in std::iter::once(&model_group.main)
        .chain(&model_group.fallbacks)
        .enumerate()
    {
        let provider = match model_group.provider(model) {
            Ok(provider) => provider,
            Err(error) => {
                failures.push(error.for_model(model, 0));
                continue;
            }
        };
        let max_tokens = overrides.max_tokens.or(model.request_settings.max_tokens);
        let temperature = overrides.temperature.or(model.request_settings.temperature);
        match model_group.validate_complete_input(
            &history,
            system_prompt,
            &tools,
            max_tokens.unwrap_or(model_group.inference.default_max_tokens) as usize,
            if index == 0 {
                input_token_estimate
            } else {
                None
            },
        ) {
            Ok(()) => {}
            Err(error) => {
                failures.push(error.for_model(model, 0));
                continue;
            }
        };
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
            &tools,
            event_tx,
            cancel_token,
            accumulated_text,
            observations,
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
    provider: Arc<dyn frona_harness::inference::provider::ModelProvider>,
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
    observations: Observations<'_>,
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
        observations.attempt(
            model_config,
            contents_result.as_ref().map_or(AttemptOutcome::Cancelled, AttemptOutcome::from_result),
            contents_result.as_ref().and_then(|result| result.as_ref().ok()).map(|output| &output.usage),
            duration_ms,
        ).await;

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
                    observations
                        .usage(model_config, Some(&output.usage), fallback_index, latency)
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
                observations
                    .usage(model_config, Some(&output.usage), fallback_index, latency)
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
