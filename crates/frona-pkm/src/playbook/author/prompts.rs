use crate::Error;
use frona_text::prompt::PromptTemplate;

/// Embedded defaults for this phase. The host may replace any template.
#[derive(Debug, Clone)]
pub struct PlaybookAuthorPrompts {
    pub system: String,
    pub input: String,
    pub reject: String,
}
impl Default for PlaybookAuthorPrompts {
    fn default() -> Self {
        Self {
            system: include_str!("../../../resources/playbook-author/system.md").into(),
            input: include_str!("../../../resources/playbook-author/input.md").into(),
            reject: include_str!("../../../resources/playbook-author/reject.md").into(),
        }
    }
}
impl PlaybookAuthorPrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system = PromptTemplate::new("pkm/playbook-author/system.md", &[])
            .render_source(&self.system, &[])?;
        let input = PromptTemplate::new(
            "pkm/playbook-author/input.md",
            &[
                "path",
                "name",
                "description",
                "body",
                "memories",
                "transcript",
                "invocations",
            ],
        )
        .render_source(&self.input, vars)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(
            PromptTemplate::new("pkm/playbook-author/reject.md", &["rejections"])
                .render_source(&self.reject, vars)?,
        )
    }
}
