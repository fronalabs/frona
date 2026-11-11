use crate::Error;
use frona_text::prompt::PromptTemplate;

/// Embedded defaults for this phase. The host may replace any template.
#[derive(Debug, Clone)]
pub struct AssemblePrompts {
    pub system: String,
    pub input: String,
    pub reject: String,
    pub bad_term: String,
}
impl Default for AssemblePrompts {
    fn default() -> Self {
        Self {
            system: include_str!("../../resources/assemble/system.md").into(),
            input: include_str!("../../resources/assemble/input.md").into(),
            bad_term: include_str!("../../resources/assemble/bad_term.md").into(),
            reject: include_str!("../../resources/assemble/reject.md").into(),
        }
    }
}
impl AssemblePrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system =
            PromptTemplate::new("pkm/assemble/system.md", &[]).render_source(&self.system, &[])?;
        let input = PromptTemplate::new("pkm/assemble/input.md", &["proposals", "axioms"])
            .render_source(&self.input, vars)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(
            PromptTemplate::new("pkm/assemble/reject.md", &["rejections"])
                .render_source(&self.reject, vars)?,
        )
    }
    pub(super) fn bad_term(&self, terms: &str) -> Result<String, Error> {
        Ok(PromptTemplate::new("pkm/assemble/bad_term.md", &["terms"])
            .render_source(&self.bad_term, &[("terms", terms)])?)
    }
}
