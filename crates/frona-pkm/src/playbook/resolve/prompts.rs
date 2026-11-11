use crate::Error;
use frona_text::prompt::PromptTemplate;

/// Embedded defaults for this phase. The host may replace any template.
#[derive(Debug, Clone)]
pub struct PlaybookResolvePrompts {
    pub system: String,
    pub input: String,
    pub reject: String,
}
impl Default for PlaybookResolvePrompts {
    fn default() -> Self {
        Self {
            system: include_str!("../../../resources/playbook-resolve/system.md").into(),
            input: include_str!("../../../resources/playbook-resolve/input.md").into(),
            reject: include_str!("../../../resources/playbook-resolve/reject.md").into(),
        }
    }
}
impl PlaybookResolvePrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system = PromptTemplate::new("pkm/playbook-resolve/system.md", &[])
            .render_source(&self.system, &[])?;
        let input = PromptTemplate::new(
            "pkm/playbook-resolve/input.md",
            &["candidate", "memories", "existing_playbooks"],
        )
        .render_source(&self.input, vars)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(
            PromptTemplate::new("pkm/playbook-resolve/reject.md", &["rejections"])
                .render_source(&self.reject, vars)?,
        )
    }
}
