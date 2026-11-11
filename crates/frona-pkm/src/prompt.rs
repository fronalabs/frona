use std::collections::HashMap;

use crate::Error;
use crate::model::{EvidenceSource, MemoryEvidence};

/// Bidirectional identifiers scoped to one model conversation.
///
/// Durable ids never cross the prompt boundary. Accepted aliases are expanded before
/// validation, checkpointing or persistence; a resumed conversation simply builds a
/// fresh deterministic map from its durable inputs.
#[derive(Debug, Clone)]
pub(crate) struct PromptIds {
    prefix: &'static str,
    real_to_local: HashMap<String, String>,
    local_to_real: HashMap<String, String>,
}

impl PromptIds {
    pub(crate) fn new(prefix: &'static str, reals: impl IntoIterator<Item = String>) -> Self {
        let mut out = Self {
            prefix,
            real_to_local: HashMap::new(),
            local_to_real: HashMap::new(),
        };
        for real in reals {
            if out.real_to_local.contains_key(&real) {
                continue;
            }
            let local = format!("{prefix}{}", out.real_to_local.len() + 1);
            out.real_to_local.insert(real.clone(), local.clone());
            out.local_to_real.insert(local, real);
        }
        out
    }

    pub(crate) fn local<'a>(&'a self, real: &'a str) -> &'a str {
        self.real_to_local
            .get(real)
            .map(String::as_str)
            .unwrap_or(real)
    }

    pub(crate) fn expand(&self, local: &str) -> Result<String, Error> {
        self.local_to_real
            .get(local.trim())
            .cloned()
            .ok_or_else(|| {
                Error::Internal(format!(
                    "unknown model-local {} id `{}`",
                    self.prefix,
                    local.trim()
                ))
            })
    }

    pub(crate) fn expand_all(&self, values: &mut [String]) -> Result<(), Error> {
        for value in values.iter_mut() {
            *value = self.expand(value)?;
        }
        Ok(())
    }
}

/// Evidence projection for prompts: retain epistemic value, never infrastructure ids.
pub(crate) fn prompt_evidence(evidence: &[MemoryEvidence]) -> String {
    evidence
        .iter()
        .map(|item| {
            let (source, quote) = match &item.source {
                EvidenceSource::UserMessage { quote, .. } => ("UserMessage", quote.as_str()),
                EvidenceSource::UserConfirmation { quote, .. } => {
                    ("UserConfirmation", quote.as_str())
                }
                EvidenceSource::AgentMessage { quote, .. } => ("AgentMessage", quote.as_str()),
                EvidenceSource::WebSearch { quote, .. } => ("WebSearch", quote.as_str()),
                EvidenceSource::WebPage { quote, .. } => ("WebPage", quote.as_str()),
                EvidenceSource::ToolResult { quote, .. } => ("ToolResult", quote.as_str()),
                EvidenceSource::TaskLifecycle { .. } => ("TaskLifecycle", ""),
                EvidenceSource::HumanEdit { quote, .. } => ("HumanEdit", quote.as_str()),
                EvidenceSource::ExternalNote { quote, .. } => ("ExternalNote", quote.as_str()),
            };
            if quote.trim().is_empty() {
                format!("{source}/{:?}", item.strength)
            } else {
                format!("{source}/{:?} quote={:?}", item.strength, quote)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests;
