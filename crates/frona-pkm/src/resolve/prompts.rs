use crate::Error;
use frona_text::prompt::PromptTemplate;
#[derive(Clone, Debug)]
pub struct ResolvePrompts {
    pub system: String,
    pub input: String,
    pub reject: String,
}
impl Default for ResolvePrompts {
    fn default() -> Self {
        Self {
            system: include_str!("../../resources/resolve/system.md").into(),
            input: include_str!("../../resources/resolve/input.md").into(),
            reject: include_str!("../../resources/resolve/reject.md").into(),
        }
    }
}
impl ResolvePrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system =
            PromptTemplate::new("pkm/resolve/system.md", &[]).render_source(&self.system, &[])?;
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
        .render_source(&self.input, vars)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(PromptTemplate::new(
            "pkm/resolve/reject.md",
            &["proposed", "subject", "candidates"],
        )
        .render_source(&self.reject, vars)?)
    }
}
