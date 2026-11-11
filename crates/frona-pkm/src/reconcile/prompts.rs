use crate::Error;
use frona_text::prompt::PromptTemplate;

/// Reconcile-specific templates. Hosts own loading/reloading any overrides.
#[derive(Debug, Clone)]
pub struct ReconcilePrompts {
    pub system: String,
    pub input: String,
    pub reject: String,
    pub promote: String,
}

impl Default for ReconcilePrompts {
    fn default() -> Self {
        Self {
            system: include_str!("../../resources/reconcile/system.md").into(),
            input: include_str!("../../resources/reconcile/input.md").into(),
            reject: include_str!("../../resources/reconcile/reject.md").into(),
            promote: include_str!("../../resources/reconcile/promote.md").into(),
        }
    }
}

impl ReconcilePrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system =
            PromptTemplate::new("pkm/reconcile/system.md", &[]).render_source(&self.system, &[])?;
        let input = PromptTemplate::new(
            "pkm/reconcile/input.md",
            &[
                "path",
                "kind",
                "name",
                "description",
                "attributes",
                "relations",
                "memories",
            ],
        )
        .render_source(&self.input, vars)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, rejections: &str) -> Result<String, Error> {
        Ok(
            PromptTemplate::new("pkm/reconcile/reject.md", &["rejections"])
                .render_source(&self.reject, &[("rejections", rejections)])?,
        )
    }
    pub(super) fn advisory(&self, suggestions: &str) -> Result<String, Error> {
        Ok(
            PromptTemplate::new("pkm/reconcile/promote.md", &["suggestions"])
                .render_source(&self.promote, &[("suggestions", suggestions)])?,
        )
    }
}
