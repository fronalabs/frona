/// The LLM write-back schema - memory ops distilled from a human's diff.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(super) struct WriteBackOps {
    #[serde(default)]
    pub(super) ops: Vec<WbOp>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub(super) struct WbOp {
    /// `add` | `supersede` | `outdated` | `wrong`.
    pub(super) op: String,
    #[serde(default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) content: String,
    #[serde(default)]
    pub(super) memory_id: String,
    #[serde(default)]
    pub(super) note: String,
}
