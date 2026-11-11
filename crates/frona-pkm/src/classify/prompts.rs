use crate::{Error, ontology::PrefixMap};
use frona_text::prompt::PromptTemplate;

#[derive(Debug, Clone)]
pub struct ClassifyPrompts {
    /// Additional rendering context; invocation variables take precedence.
    pub defaults: std::collections::HashMap<String, String>,
    pub system: String,
    pub input: String,
    pub reject: String,
    pub bad_term: String,
}
impl Default for ClassifyPrompts {
    fn default() -> Self {
        Self {
            defaults: Default::default(),
            system: include_str!("../../resources/classify/system.md").into(),
            input: include_str!("../../resources/classify/input.md").into(),
            reject: include_str!("../../resources/classify/reject.md").into(),
            bad_term: include_str!("../../resources/classify/bad_term.md").into(),
        }
    }
}
impl ClassifyPrompts {
    pub(super) fn render(&self, vars: &[(&str, &str)]) -> Result<(String, String), Error> {
        let system = PromptTemplate::new("pkm/classify/system.md", &[])
            .render_source_with_defaults(&self.system, &[], &self.defaults)?;
        let input = PromptTemplate::new(
            "pkm/classify/input.md",
            &[
                "name",
                "description",
                "contributions",
                "identity_evidence",
                "facts",
                "relations",
                "attributes",
                "minted",
                "evidence",
            ],
        )
        .render_source_with_defaults(&self.input, vars, &self.defaults)?;
        Ok((system, input))
    }
    pub(super) fn reject(&self, vars: &[(&str, &str)]) -> Result<String, Error> {
        Ok(
            PromptTemplate::new("pkm/classify/reject.md", &["violations", "class"])
                .render_source_with_defaults(&self.reject, vars, &self.defaults)?,
        )
    }
    pub(super) fn bad_term(
        &self,
        prefixes: &PrefixMap,
        terms: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Option<String>, Error> {
        let bad: Vec<_> = terms
            .into_iter()
            .filter_map(|term| prefixes.validate_term(term.as_ref()).err())
            .map(|error| format!("- {error}"))
            .collect();
        if bad.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                PromptTemplate::new("pkm/classify/bad_term.md", &["terms"])
                    .render_source_with_defaults(
                        &self.bad_term,
                        &[("terms", &bad.join("\n"))],
                        &self.defaults,
                    )?,
            ))
        }
    }
}
