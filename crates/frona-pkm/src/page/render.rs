use crate::model::{KnowledgeEntity, KnowledgeMemory};
use crate::prompt::prompt_evidence;
use frona_harness::inference::InferenceError;
use frona_harness::{FailureKind, ManualError, ModelError, RunOutcome};
/// Replacement is a domain choice for an unavailable provider or empty answer.
/// Typed execution controls (including nested provider stops), setup, compaction,
/// and protocol failures never become a successful deterministic page.
pub(super) fn replacement_allowed(reason: &ManualError, outcome: &Option<RunOutcome>) -> bool {
    matches!(reason, ManualError::NoText)
        || (matches!(reason, ManualError::Model(ModelError::Provider(error))
            if provider_replacement_allowed(error))
            && matches!(
                outcome,
                Some(RunOutcome::Incomplete {
                    kind: FailureKind::Model,
                    ..
                })
            ))
}

/// Prepared groups wrap both operational and setup failures with provider/fallback
/// provenance. Only an aggregate of operational failures permits replacement.
pub(super) fn provider_replacement_allowed(error: &InferenceError) -> bool {
    match error {
        InferenceError::InferenceFailed(_)
        | InferenceError::StreamingFailed(_)
        | InferenceError::CompletionFailed(_)
        | InferenceError::RateLimited { .. }
        | InferenceError::EmptyResponse => true,
        InferenceError::ModelFailed { source, .. } => provider_replacement_allowed(source),
        InferenceError::AllFallbacksFailed(errors) => {
            !errors.is_empty() && errors.iter().all(provider_replacement_allowed)
        }
        InferenceError::ProviderNotConfigured(_)
        | InferenceError::ModelGroupNotFound(_)
        | InferenceError::ConfigError(_)
        | InferenceError::InvalidOutput { .. }
        | InferenceError::Stop(_)
        | InferenceError::Cancelled(_) => false,
    }
}

pub(super) fn deterministic_body(
    page: &KnowledgeEntity,
    current: &[&KnowledgeMemory],
    timezone: &str,
) -> String {
    let mut s = String::new();
    if !page.description.trim().is_empty() {
        s.push_str(page.description.trim());
        s.push_str("\n\n");
    }
    if !current.is_empty() {
        s.push_str("## What we know\n\n");
        for m in current {
            s.push_str(&author_memory_bullet(m, timezone));
        }
    }
    s
}

pub(super) fn list_or(memories: &[&KnowledgeMemory], empty: &str, timezone: &str) -> String {
    if memories.is_empty() {
        return format!("{empty}\n");
    }
    let mut s = String::new();
    for m in memories {
        s.push_str(&author_memory_bullet(m, timezone));
    }
    s
}

fn author_memory_bullet(memory: &KnowledgeMemory, timezone: &str) -> String {
    let evidence = prompt_evidence(&memory.evidence);
    let mut rendered = format!(
        "- ({:?}) {}\n  evidence: {}\n",
        memory.kind, memory.content, evidence,
    );
    if let Some(episode) = &memory.episode {
        let status = format!("{:?}", episode.status).to_ascii_lowercase();
        let mut fields = vec![format!("status={status}")];
        if let Some(start) = episode.resolved_start {
            fields.push(format!(
                "start_utc={}",
                start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ));
            let tz = timezone.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
            fields.push(format!(
                "start_local={} timezone={timezone}",
                start
                    .with_timezone(&tz)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            ));
        } else if let Some(absolute) = &episode.absolute {
            fields.push(format!(
                "absolute_utc={}",
                serde_json::to_string(absolute).unwrap_or_else(|_| "null".into()),
            ));
        }
        if let Some(end) = episode.resolved_end {
            fields.push(format!(
                "end_utc={}",
                end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ));
            let tz = timezone.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
            fields.push(format!(
                "end_local={} timezone={timezone}",
                end.with_timezone(&tz)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            ));
        }
        if let Some(duration) = &episode.duration {
            fields.push(format!(
                "duration={}",
                serde_json::to_string(duration).unwrap_or_else(|_| "null".into()),
            ));
        }
        rendered.push_str(&format!("  episode: {}\n", fields.join("; ")));
    }
    rendered
}
