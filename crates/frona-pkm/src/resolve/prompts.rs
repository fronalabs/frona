use crate::Error;
use frona_text::prompt::PromptTemplate;
#[derive(Clone, Debug)]
pub struct ResolvePrompts {
    /// Additional rendering context; invocation variables take precedence.
    pub defaults: std::collections::HashMap<String, String>,
    pub system: String,
    pub input: String,
    pub reject: String,
}
impl Default for ResolvePrompts {
    fn default() -> Self {
        Self {
            defaults: Default::default(),
            system: include_str!("../../resources/resolve/system.md").into(),
            input: include_str!("../../resources/resolve/input.md").into(),
            reject: include_str!("../../resources/resolve/reject.md").into(),
        }
    }
}
impl ResolvePrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system = PromptTemplate::new("pkm/resolve/system.md", &[])
            .render_source_with_defaults(&self.system, &[], &self.defaults)?;
        let input = PromptTemplate::new(
            "pkm/resolve/input.md",
            &[
                "path",
                "name",
                "aliases",
                "description",
                "kind",
                "identity_evidence",
                "assertions",
                "candidates",
            ],
        )
        .render_source_with_defaults(&self.input, vars, &self.defaults)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(PromptTemplate::new(
            "pkm/resolve/reject.md",
            &["proposed", "subject", "candidates"],
        )
        .render_source_with_defaults(&self.reject, vars, &self.defaults)?)
    }
}
