//! The ontology tool surface for consolidation. These are
//! **consolidation-internal** tools (like the playbook `get_invocation_output`):
//! built per pass, scoped to one `user_id`, and handed to model stages as
//! `extra_tools` - never part of the Cedar agent registry. All are read-only over
//! the reasoned graph except `test_edit`, which is a dry run (persists nothing).

use std::collections::HashSet;
use std::sync::Arc;

use oxigraph::sparql::QueryResults;
use oxrdf::Triple;
use serde_json::Value;

use super::{allowance::ResearchAllowance, str_arg};
use crate::Error;
use frona_harness::{ToolOutput, ToolRegistry};

use crate::ontology::sparql::{self, term_lexical};
use crate::ontology::{Ontology, PrefixMap, SchemaEdit, ValidationDiagnostic};

#[derive(Clone)]
pub struct OntologyToolOverlay {
    pub entities: Vec<crate::model::KnowledgeEntity>,
    pub proposed_edits: Vec<SchemaEdit>,
    pub abox: Vec<Triple>,
    pub diagnostics:
        Arc<std::sync::RwLock<std::collections::HashMap<String, ValidationDiagnostic>>>,
    pub prefixes: PrefixMap,
    pub allowance: ResearchAllowance,
}

fn spend_tool_call(overlay: &Option<Arc<OntologyToolOverlay>>) -> Option<ToolOutput> {
    let overlay = overlay.as_ref()?;
    (!overlay.allowance.try_acquire()).then(|| ToolOutput::text(format!(
        "Research-tool budget exhausted ({} calls). Submit the best complete decision set now; unresolved terms will keep their validated proposals.",
        overlay.allowance.limit(),
    )))
}

#[derive(Clone, Copy)]
pub(crate) enum OntologyToolProfile {
    Classify,
    Resolve,
    Assemble,
}

impl OntologyToolProfile {
    const fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Classify => &[
                "ontology_term_search",
                "inspect_ontology_terms",
                "search_entities",
                "read_entity",
                "validation_details",
                "test_edit",
            ],
            Self::Resolve => &["inspect_ontology_terms", "search_entities", "read_entity"],
            Self::Assemble => &[
                "ontology_sparql",
                "ontology_term_search",
                "inspect_ontology_terms",
                "search_entities",
                "read_entity",
                "usage_impact",
                "validation_details",
                "test_edit",
            ],
        }
    }

    fn includes(self, tool: &str) -> bool {
        self.tool_names().contains(&tool)
    }
}

pub(crate) fn build_ontology_tools_with_overlay(
    manager: Ontology,
    context: &crate::GraphSnapshot,
    prefixes: PrefixMap,
    overlay: Option<Arc<OntologyToolOverlay>>,
    profile: OntologyToolProfile,
) -> Result<ToolRegistry<'static>, Error> {
    let repo = context.clone();
    let view = context.clone();
    let user_id = context.scope_id.clone();
    let page_overlay = overlay.as_ref().map(|overlay| {
        Arc::new(crate::tools::EntityToolView::new(
            overlay.entities.clone(),
            overlay.allowance.clone(),
        ))
    });
    let mut tools = ToolRegistry::default();
    if profile.includes("ontology_sparql") {
        tools
            .register(
                super::PortableTool::new(
                    OntologySparqlTool {
                        manager: manager.clone(),
                        user_id: user_id.clone(),
                        prefixes: prefixes.clone(),
                        overlay: overlay.clone(),
                    },
                    "ontology_sparql",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("ontology_term_search") {
        tools
            .register(
                super::PortableTool::new(
                    OntologyTermSearchTool {
                        manager: manager.clone(),
                        user_id: user_id.clone(),
                        prefixes: prefixes.clone(),
                        overlay: overlay.clone(),
                    },
                    "ontology_term_search",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("inspect_ontology_terms") {
        tools
            .register(
                super::PortableTool::new(
                    InspectOntologyTermsTool {
                        manager: manager.clone(),
                        user_id: user_id.clone(),
                        prefixes: prefixes.clone(),
                        overlay: overlay.clone(),
                    },
                    "inspect_ontology_terms",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("search_entities") {
        tools
            .register(
                super::PortableTool::new(
                    crate::tools::SearchEntitiesTool {
                        repo: view.clone(),
                        overlay: page_overlay.clone(),
                        budget: None,
                        prefixes: prefixes.clone(),
                    },
                    "search_entities",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("read_entity") {
        tools
            .register(
                super::PortableTool::new(
                    crate::tools::ReadEntityTool {
                        repo: view,
                        memories: repo,
                        overlay: page_overlay,
                        budget: None,
                        prefixes: prefixes.clone(),
                    },
                    "read_entity",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("usage_impact") {
        tools
            .register(
                super::PortableTool::new(
                    UsageImpactTool {
                        manager: manager.clone(),
                        user_id: user_id.clone(),
                        overlay: overlay.clone(),
                    },
                    "usage_impact",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("validation_details") {
        tools
            .register(
                super::PortableTool::new(
                    ValidationDetailsTool {
                        overlay: overlay.clone(),
                    },
                    "validation_details",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    if profile.includes("test_edit") {
        tools
            .register(
                super::PortableTool::new(
                    TestEditTool {
                        manager,
                        user_id,
                        overlay,
                    },
                    "test_edit",
                )
                .map_err(|e| Error::Internal(e.to_string()))?,
            )
            .map_err(|e| Error::Internal(e.to_string()))?;
    }
    Ok(tools)
}

pub(crate) struct ValidationDetailsTool {
    pub overlay: Option<Arc<OntologyToolOverlay>>,
}

impl ValidationDetailsTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_tool_call(&self.overlay) {
            return Ok(output);
        }
        let Some(id) = str_arg(&arguments, "diagnostic_id") else {
            return Ok(ToolOutput::text(
                "Provide a `diagnostic_id` from validation feedback.",
            ));
        };
        let Some(overlay) = &self.overlay else {
            return Ok(ToolOutput::text("No active projection diagnostics."));
        };
        let diagnostics = overlay
            .diagnostics
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(diagnostic) = diagnostics.get(id) else {
            return Ok(ToolOutput::text(format!("Unknown diagnostic `{id}`.")));
        };
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(diagnostic).unwrap_or_else(|_| diagnostic.detail.clone()),
        ))
    }
}

const SPARQL_ROW_CAP: usize = 200;
const VOCAB_HIT_CAP: usize = 25;
const TERM_INSPECTION_CAP: usize = 10;

/// `ontology_sparql(query)` - run a read-only SPARQL query over the user's
/// materialized (reasoned) knowledge graph: TBox classes *and* ABox individuals,
/// one endpoint. Subsumes schema-lookup / entity-find / relation-traverse.
pub(crate) struct OntologySparqlTool {
    pub manager: Ontology,
    pub user_id: String,
    pub prefixes: PrefixMap,
    pub overlay: Option<Arc<OntologyToolOverlay>>,
}

impl OntologySparqlTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_tool_call(&self.overlay) {
            return Ok(output);
        }
        let Some(query) = str_arg(&arguments, "query") else {
            return Ok(ToolOutput::text("Provide a `query` (a SPARQL SELECT/ASK)."));
        };
        let result = if let Some(overlay) = &self.overlay {
            self.manager
                .reason_user_with_proposed(&self.user_id, &overlay.proposed_edits, &overlay.abox)
                .await
                .and_then(|pass| sparql::query(&pass.reasoned.store, query, &self.prefixes))
        } else {
            self.manager.sparql(&self.user_id, query).await
        };
        match result {
            Ok(results) => Ok(ToolOutput::text(format_results(results, SPARQL_ROW_CAP))),
            // A query error is the model's to fix - surface it, don't fail the loop.
            Err(e) => Ok(ToolOutput::text(format!("query error: {e}"))),
        }
    }
}

/// `ontology_term_search(term)` - search the user's active and declared terms together
/// with the whole catalogue. Lexical quality remains primary; user relevance breaks ties.
pub(crate) struct OntologyTermSearchTool {
    pub manager: Ontology,
    pub user_id: String,
    pub prefixes: PrefixMap,
    pub overlay: Option<Arc<OntologyToolOverlay>>,
}

impl OntologyTermSearchTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_tool_call(&self.overlay) {
            return Ok(output);
        }
        let Some(term) = str_arg(&arguments, "term") else {
            return Ok(ToolOutput::text("Provide a `term` to search the ontology."));
        };
        let active = active_terms(&self.overlay, &self.prefixes);
        let proposed = self
            .overlay
            .as_ref()
            .map(|overlay| overlay.proposed_edits.as_slice())
            .unwrap_or(&[]);
        let hits = self
            .manager
            .search_ontology_terms(&self.user_id, proposed, &active, term, VOCAB_HIT_CAP)
            .await?;
        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No ontology term matches \"{term}\". If nothing fits, mint a frona: term."
            )));
        }
        let lines: Vec<String> = hits
            .iter()
            .map(|hit| match &hit.label {
                Some(label) => format!(
                    "{} [{}; {}; {}] — {label}",
                    hit.term, hit.kind, hit.user_relevance, hit.origin,
                ),
                None => format!(
                    "{} [{}; {}; {}]",
                    hit.term, hit.kind, hit.user_relevance, hit.origin,
                ),
            })
            .collect();
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

/// `inspect_ontology_terms(terms)` - inspect a bounded class/property hierarchy slice
/// over the whole catalogue plus this user's committed and proposed schema.
pub(crate) struct InspectOntologyTermsTool {
    pub manager: Ontology,
    pub user_id: String,
    pub prefixes: PrefixMap,
    pub overlay: Option<Arc<OntologyToolOverlay>>,
}

impl InspectOntologyTermsTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_tool_call(&self.overlay) {
            return Ok(output);
        }
        let Some(values) = arguments.get("terms").and_then(Value::as_array) else {
            return Ok(ToolOutput::text("Provide `terms` as an array of CURIEs."));
        };
        let mut terms: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(str::to_string)
            .collect();
        terms.dedup();
        if terms.is_empty() {
            return Ok(ToolOutput::text("Provide at least one ontology term."));
        }
        if terms.len() > TERM_INSPECTION_CAP {
            return Ok(ToolOutput::text(format!(
                "Inspect at most {TERM_INSPECTION_CAP} terms in one call."
            )));
        }
        let active = active_terms(&self.overlay, &self.prefixes);
        let proposed = self
            .overlay
            .as_ref()
            .map(|overlay| overlay.proposed_edits.as_slice())
            .unwrap_or(&[]);
        let (terms, comparisons) = self
            .manager
            .inspect_ontology_terms(&self.user_id, proposed, &active, &terms)
            .await?;
        let result = serde_json::json!({ "terms": terms, "comparisons": comparisons });
        Ok(ToolOutput::text(
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
        ))
    }
}

fn active_terms(
    overlay: &Option<Arc<OntologyToolOverlay>>,
    prefixes: &PrefixMap,
) -> HashSet<String> {
    let Some(overlay) = overlay else {
        return HashSet::new();
    };
    let mut active = HashSet::new();
    for entity in &overlay.entities {
        active.extend(entity.kinds.iter().map(|term| prefixes.expand(term)));
        if let Some(attributes) = entity.attributes.as_object() {
            active.extend(attributes.keys().map(|term| prefixes.expand(term)));
        }
    }
    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
    for triple in &overlay.abox {
        if triple.predicate.as_str() == RDF_TYPE {
            if let oxrdf::Term::NamedNode(class) = &triple.object {
                active.insert(class.as_str().to_string());
            }
        } else {
            active.insert(triple.predicate.as_str().to_string());
        }
    }
    active
}

/// `usage_impact(term)` - how many entities/links currently use a class or relation
/// (the blast radius of a rename / re-typing).
pub(crate) struct UsageImpactTool {
    pub manager: Ontology,
    pub user_id: String,
    pub overlay: Option<Arc<OntologyToolOverlay>>,
}

impl UsageImpactTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_tool_call(&self.overlay) {
            return Ok(output);
        }
        let Some(term) = str_arg(&arguments, "term") else {
            return Ok(ToolOutput::text(
                "Provide a `term` (a class or relation CURIE).",
            ));
        };
        let (entities, links) = if let Some(overlay) = &self.overlay {
            let px = &overlay.prefixes;
            let expanded = px.expand(term);
            let entities = overlay
                .entities
                .iter()
                .filter(|entity| entity.kinds.iter().any(|kind| px.expand(kind) == expanded))
                .count();
            let links = overlay
                .abox
                .iter()
                .filter(|triple| triple.predicate.as_str() == expanded)
                .count();
            (entities, links)
        } else {
            self.manager.usage_impact(&self.user_id, term).await?
        };
        Ok(ToolOutput::text(format!(
            "{term}: {entities} entity(s) typed with it, {links} link(s) using it."
        )))
    }
}

/// `test_edit(edits)` - dry-run proposed schema edits: reports any logical clash an
/// edit would introduce (an unsatisfiable class, a disjointness violation). Nothing
/// is persisted.
pub(crate) struct TestEditTool {
    pub manager: Ontology,
    pub user_id: String,
    pub overlay: Option<Arc<OntologyToolOverlay>>,
}

impl TestEditTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        if let Some(output) = spend_tool_call(&self.overlay) {
            return Ok(output);
        }
        let Some(edits_json) = arguments.get("edits") else {
            return Ok(ToolOutput::text(
                "Provide `edits` (an array of schema edits).",
            ));
        };
        let edits: Vec<SchemaEdit> = match serde_json::from_value(edits_json.clone()) {
            Ok(e) => e,
            Err(e) => return Ok(ToolOutput::text(format!("could not parse edits: {e}"))),
        };
        let impact = if let Some(overlay) = &self.overlay {
            let mut combined = overlay.proposed_edits.clone();
            for edit in edits {
                if !combined.contains(&edit) {
                    combined.push(edit);
                }
            }
            self.manager
                .test_edits_with_abox(&self.user_id, &combined, &overlay.abox)
                .await?
        } else {
            self.manager.test_edits(&self.user_id, &edits).await?
        };
        let mut report = String::new();
        if !impact.incoherence.is_empty() {
            report.push_str(&format!(
                "INCOHERENT — the schema would be unsatisfiable (a hard block):\n{}\n",
                impact.incoherence.join("\n")
            ));
        }
        if !impact.data_violations.is_empty() {
            report.push_str(&format!(
                "{} existing fact(s) would be flagged by these edits (isolated → they \
                 quarantine; recurring → reconsider):\n{}\n",
                impact.data_violations.len(),
                impact
                    .data_violations
                    .iter()
                    .map(|v| format!("- {}", v.detail))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if report.is_empty() {
            Ok(ToolOutput::text(
                "consistent — the edits introduce no clash and break no existing data.",
            ))
        } else {
            Ok(ToolOutput::text(report))
        }
    }
}

fn format_results(results: QueryResults<'static>, cap: usize) -> String {
    match results {
        QueryResults::Boolean(b) => b.to_string(),
        QueryResults::Solutions(sols) => {
            let vars: Vec<String> = sols
                .variables()
                .iter()
                .map(|v| v.as_str().to_string())
                .collect();
            let mut lines = vec![vars.join("\t")];
            for (i, sol) in sols.enumerate() {
                if i >= cap {
                    lines.push(format!("… (capped at {cap} rows)"));
                    break;
                }
                let Ok(sol) = sol else { continue };
                let row: Vec<String> = vars
                    .iter()
                    .map(|v| sol.get(v.as_str()).map(term_lexical).unwrap_or_default())
                    .collect();
                lines.push(row.join("\t"));
            }
            if lines.len() == 1 {
                lines.push("(no rows)".into());
            }
            lines.join("\n")
        }
        QueryResults::Graph(_) => "(CONSTRUCT/DESCRIBE graph results are not supported)".into(),
    }
}

#[async_trait::async_trait]
impl super::Handler for ValidationDetailsTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        ValidationDetailsTool::execute(self, arguments).await
    }
}

#[async_trait::async_trait]
impl super::Handler for OntologySparqlTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        OntologySparqlTool::execute(self, arguments).await
    }
}

#[async_trait::async_trait]
impl super::Handler for OntologyTermSearchTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        OntologyTermSearchTool::execute(self, arguments).await
    }
}

#[async_trait::async_trait]
impl super::Handler for InspectOntologyTermsTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        InspectOntologyTermsTool::execute(self, arguments).await
    }
}

#[async_trait::async_trait]
impl super::Handler for UsageImpactTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        UsageImpactTool::execute(self, arguments).await
    }
}

#[async_trait::async_trait]
impl super::Handler for TestEditTool {
    async fn execute(&self, arguments: Value) -> Result<ToolOutput, Error> {
        TestEditTool::execute(self, arguments).await
    }
}

pub(crate) fn overlay(
    ontology: &Ontology,
    graph: &crate::GraphSnapshot,
    proposals: &crate::classify::ProposalSet,
    tool_budget: usize,
) -> Result<Arc<OntologyToolOverlay>, Error> {
    let entities = graph
        .snapshot_with(&proposals.entity_draft())?
        .into_entities();
    let (projected, links) = proposals.project_graph(
        &graph.scope_id,
        graph.knowledge_entities(),
        graph.asserted_links(),
    );
    Ok(Arc::new(OntologyToolOverlay {
        entities,
        proposed_edits: proposals.proposed_edits.clone(),
        abox: ontology.assertion_graph(&projected, &links),
        diagnostics: Default::default(),
        prefixes: ontology.prefixes(),
        allowance: ResearchAllowance::new(tool_budget),
    }))
}
