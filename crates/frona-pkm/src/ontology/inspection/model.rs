use serde::Serialize;

pub struct OntologyExport {
    pub tbox: Vec<oxrdf::Triple>,
    pub abox: Vec<oxrdf::Triple>,
    pub entity_count: usize,
    pub asserted_link_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OntologySearchHit {
    pub term: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub origin: String,
    pub user_relevance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OntologyPropertyInspection {
    pub domain: Vec<String>,
    pub range: Vec<String>,
    pub inverse: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OntologyTermInspection {
    pub term: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub user_relevance: String,
    pub direct_parents: Vec<String>,
    pub ancestors: Vec<String>,
    pub direct_children: Vec<String>,
    pub children_truncated: bool,
    pub equivalents: Vec<String>,
    pub disjoint_with: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property: Option<OntologyPropertyInspection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OntologyTermRelation {
    pub a: String,
    pub b: String,
    pub relation: String,
}
