//! Opt-in transcript tracing: every LLM request and its response, on disk, as JSON.
//!
//! `tracing::debug!` already logs both, but `Debug`-formats a whole multi-turn history
//! into one line, which is unreadable exactly when it matters, since the question is
//! usually *what the model was actually shown* several turns in. This writes one file per
//! call instead, with the system prompt, every message in order, the exact tool definitions
//! offered, and what came back.
//!
//! In debug builds, the first call reads the environment and initializes one static trace
//! configuration. Later calls only read that cached value. In release builds, this module
//! provides no-op functions and does not read the environment or write trace files.
//!
//! ```text
//! FRONA_LLM_TRACE=/tmp/llm cargo run -p frona-server --bin frona
//! ```
//!
//! Files are `0001-<model>.json`, numbered in call order. **The trace contains whatever
//! was sent to the model**, including transcripts, page content, and tool output, so it
//! belongs in a scratch directory, not anywhere that gets shared.

#[cfg(debug_assertions)]
use std::path::PathBuf;

#[cfg(debug_assertions)]
use std::sync::LazyLock;

#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicUsize, Ordering};

use rig_core::completion::request::ToolDefinition as RigToolDefinition;
use rig_core::completion::{AssistantContent, Message as RigMessage};

#[cfg(debug_assertions)]
struct TraceConfig {
    primary: PathBuf,
    mirror: Option<PathBuf>,
}

#[cfg(debug_assertions)]
static CONFIG: LazyLock<Option<TraceConfig>> = LazyLock::new(|| {
    let raw = std::env::var("FRONA_LLM_TRACE")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let primary = PathBuf::from(raw);
    if let Err(error) = std::fs::create_dir_all(&primary) {
        tracing::warn!(error = %error, dir = %primary.display(), "LLM trace dir unusable");
        return None;
    }
    tracing::info!(dir = %primary.display(), "LLM transcript tracing enabled");

    let mirror = std::env::var("FRONA_LLM_TRACE_MIRROR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .and_then(|path| match std::fs::create_dir_all(&path) {
            Ok(()) => Some(path),
            Err(error) => {
                tracing::warn!(error = %error, dir = %path.display(), "LLM trace mirror unusable");
                None
            }
        });

    Some(TraceConfig { primary, mirror })
});

#[cfg(debug_assertions)]
static SEQ: AtomicUsize = AtomicUsize::new(0);

#[cfg(debug_assertions)]
static STAGE_STATE_SEQ: AtomicUsize = AtomicUsize::new(0);

#[cfg(debug_assertions)]
fn config() -> Option<&'static TraceConfig> {
    CONFIG.as_ref()
}

#[cfg(debug_assertions)]
pub fn enabled() -> bool {
    config().is_some()
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub const fn enabled() -> bool {
    false
}

#[cfg(debug_assertions)]
fn write_trace_file(config: &TraceConfig, name: &str, body: &str) {
    for directory in std::iter::once(&config.primary).chain(config.mirror.iter()) {
        let path = directory.join(name);
        if let Err(error) = std::fs::write(&path, body) {
            tracing::warn!(error = %error, path = %path.display(), "LLM trace write failed");
        }
    }
}

/// One traced exchange. `outcome` is filled in by whoever consumed the response, such as a
/// schema failure is the most useful thing a trace can record, and only the caller knows.
pub struct Exchange<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub history: &'a [RigMessage],
    pub tools: &'a [RigToolDefinition],
}

/// Write one request/response pair. Never fails the call it is tracing: a trace that
/// cannot be written is a warning, not an inference error.
#[cfg(debug_assertions)]
pub fn record(ex: Exchange<'_>, response: &[AssistantContent]) {
    let Some(config) = config() else { return };
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;

    // Keep the complete definitions, not just their names. The parameter schema is part of
    // what the model actually saw, and is often the evidence needed to distinguish a model
    // decision from a schema/prompt mismatch. In particular, PKM classify's `submit` schema
    // is where `attributes.targets` and `new_entities` become available to the model.
    let tools: Vec<&str> = ex.tools.iter().map(|t| t.name.as_str()).collect();
    let doc = serde_json::json!({
        "seq": seq,
        "consolidation_stage_state_seq": STAGE_STATE_SEQ.load(Ordering::Relaxed),
        "model": ex.model,
        "tools_offered": tools,
        "tool_definitions": ex.tools,
        "system": ex.system,
        "messages": ex.history,
        "response": response,
    });

    let safe_model: String = ex
        .model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{seq:04}-{safe_model}.json");
    let body = match serde_json::to_string_pretty(&doc) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "LLM trace serialize failed");
            return;
        }
    };
    write_trace_file(config, &name, &body);
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn record(_ex: Exchange<'_>, _response: &[AssistantContent]) {}

/// Persist one accepted PKM stage-state transition beside the request traces.
///
/// These files are deliberately separate from provider exchanges: one accepted model
/// answer can cause several externally-validated operations, while a rejected answer can
/// cause no state transition at all. Numbering the transitions independently preserves
/// both histories without pretending they are one-to-one.
#[cfg(debug_assertions)]
pub fn record_stage_state(stage: &str, phase: &str, item: &str, state: &serde_json::Value) {
    let Some(config) = config() else { return };
    let seq = STAGE_STATE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let doc = serde_json::json!({
        "seq": seq,
        "after_request_seq": SEQ.load(Ordering::Relaxed),
        "stage": stage,
        "phase": phase,
        "item": item,
        "state": state,
    });
    let name = format!("consolidation-stage-state-{seq:04}.json");
    let body = match serde_json::to_string_pretty(&doc) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(error = %e, phase, item, "stage-state trace serialize failed");
            return;
        }
    };
    write_trace_file(config, &name, &body);
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn record_stage_state(_stage: &str, _phase: &str, _item: &str, _state: &serde_json::Value) {}

#[cfg(test)]
mod tests {
    /// What `submit`'s parameter schema actually looks like on the wire. Not an
    /// assertion. This is a printout for when a model's submission shape disagrees with ours.
    #[test]
    fn print_submit_schema() {
        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Inner {
            class: String,
        }

        #[derive(schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Classification {
            classes: Vec<Inner>,
            relations: Vec<String>,
        }
        let s = serde_json::to_string_pretty(&schemars::schema_for!(Classification)).unwrap();
        println!("{s}");
    }
}
