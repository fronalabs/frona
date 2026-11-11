use crate::Error;
use frona_text::prompt::PromptTemplate;
/// Embedded phase defaults. The host can replace templates without filesystem coupling.
#[derive(Debug, Clone)]
pub struct WritebackPrompts {
    /// Additional rendering context; invocation variables take precedence.
    pub defaults: std::collections::HashMap<String, String>,
    pub system: String,
    pub input: String,
}
impl Default for WritebackPrompts {
    fn default() -> Self {
        Self {
            defaults: Default::default(),
            system: include_str!("../../resources/writeback/system.md").into(),
            input: include_str!("../../resources/writeback/input.md").into(),
        }
    }
}
impl WritebackPrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system = PromptTemplate::new("pkm/writeback/system.md", &[])
            .render_source_with_defaults(&self.system, &[], &self.defaults)?;
        let input = PromptTemplate::new("pkm/writeback/input.md", &["memories", "diff"])
            .render_source_with_defaults(&self.input, vars, &self.defaults)?;
        Ok((system, input))
    }
}
