use crate::Error;
use frona_text::prompt::PromptTemplate;

/// Embedded defaults copied from the server. Override individual templates by
/// replacing these strings; the host decides how to load or reload overrides.
#[derive(Debug, Clone)]
pub struct IngestPrompts {
    /// Additional rendering context; invocation variables take precedence.
    pub defaults: std::collections::HashMap<String, String>,
    pub system: String,
    pub input: String,
    pub reject: String,
}

impl Default for IngestPrompts {
    fn default() -> Self {
        Self {
            defaults: Default::default(),
            system: include_str!("../../resources/ingest/system.md").into(),
            input: include_str!("../../resources/ingest/input.md").into(),
            reject: include_str!("../../resources/ingest/reject.md").into(),
        }
    }
}

impl IngestPrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system = PromptTemplate::new("pkm/ingest/system.md", &[]).render_source_with_defaults(
            &self.system,
            &[],
            &self.defaults,
        )?;
        let input = PromptTemplate::new(
            "pkm/ingest/input.md",
            &[
                "owner_name",
                "handle",
                "self_path",
                "existing_entities",
                "research_messages",
                "transcript",
            ],
        )
        .render_source_with_defaults(&self.input, vars, &self.defaults)?;
        let transcript = vars
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("transcript"))
            .expect("validated transcript argument")
            .1;
        if !input.contains(transcript.trim()) {
            return Err(Error::Internal(
                "extract: rendered prompt does not contain the transcript".into(),
            ));
        }
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(PromptTemplate::new(
            "pkm/ingest/reject.md",
            &["rejected", "accepted_memories", "memory_repairs"],
        )
        .render_source_with_defaults(&self.reject, vars, &self.defaults)?)
    }
}
