use super::*;
use crate::Phase;

/// Complete, effective state of one consolidation. The host applies the returned
/// plan atomically using its ontology version, then advances its checkpoint.
#[derive(Clone)]
pub struct AssembleInput {
    pub graph: GraphSnapshot,
    pub ontology: Ontology,
    pub proposals: ProposalSet,
    pub banked_adjudication: Option<AdjudicationResult>,
    /// Losing path to its canonical replacement; chains are allowed, cycles are not.
    pub resolved_into: BTreeMap<String, String>,
    pub discarded_paths: HashSet<String>,
}
#[derive(Debug, Default)]
pub struct AttributeOps {
    pub path: String,
    pub rekeys: Vec<(String, String)>,
    pub promoted: Vec<(String, String, String)>,
    pub retracted: Vec<(String, String)>,
}
/// An uncommitted atomic write plan. CAS/reload/retry and checkpoint persistence
/// remain with the caller; a stale plan must be recomputed from fresh snapshots.
#[derive(Debug)]
pub struct AssembleOutput {
    pub expected_ontology_version: i64,
    pub ontology_delta: String,
    pub schema_triples: Vec<Triple>,
    pub edits: Vec<SchemaEdit>,
    pub types: Vec<(String, Vec<String>)>,
    pub rekeys: Vec<(String, String, String)>,
    pub relation_type_renames: Vec<(String, String)>,
    pub attributes: Vec<AttributeOps>,
    pub materialize: Vec<KnowledgeEntity>,
    pub coalesced_sources: Vec<(String, String, String)>,
    pub coalesced_aliases: Vec<(String, Vec<String>)>,
    pub working_outcomes: Vec<(String, Option<String>)>,
    pub banked_adjudication: Option<AdjudicationResult>,
    pub stamped: usize,
    pub entities_created: usize,
    pub declared: usize,
    pub aligned: usize,
    pub deferred: usize,
}
/// Host checkpoint for source decisions after independent re-gating and validation.
/// A source may include rejected decisions from a retained, incomplete conversation;
/// it is replay material, not an accepted publication plan. Replay must validate it
/// again against the current graph. Only `AssembleOutput` contains the final plan.
/// A checkpoint failure stops the phase before later batches or publication.
#[async_trait::async_trait]
pub trait AdjudicationCheckpoint: Send + Sync {
    async fn record(&self, result: &AdjudicationResult) -> Result<(), Error>;
}
struct NoCheckpoint;
#[async_trait::async_trait]
impl AdjudicationCheckpoint for NoCheckpoint {
    async fn record(&self, _result: &AdjudicationResult) -> Result<(), Error> {
        Ok(())
    }
}
pub struct Assemble<'a> {
    harness: Harness<'a>,
    prompts: AssemblePrompts,
    max_submissions: usize,
    max_tool_turns: Option<usize>,
    cancellation: tokio_util::sync::CancellationToken,
}
impl<'a> Assemble<'a> {
    pub fn new(harness: Harness<'a>) -> Self {
        Self {
            harness,
            prompts: Default::default(),
            max_submissions: 40,
            max_tool_turns: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }
    pub fn cancellation(mut self, value: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation = value;
        self
    }
    pub fn prompts(mut self, value: AssemblePrompts) -> Self {
        self.prompts = value;
        self
    }
    pub fn submission_attempts(mut self, value: usize) -> Self {
        self.max_submissions = value;
        self
    }
    pub fn exploration_turns(mut self, value: usize) -> Self {
        self.max_tool_turns = Some(value);
        self
    }
    pub async fn run(&self, input: AssembleInput) -> Result<AssembleOutput, Error> {
        self.run_with(input, |run| run).await
    }
    /// Configure each independent adjudication batch; capture shared limits to
    /// account across all batches. Small validated tails do not call the model.
    pub async fn run_with<'run, F>(
        &'run self,
        input: AssembleInput,
        configure: F,
    ) -> Result<AssembleOutput, Error>
    where
        'a: 'run,
        F: Fn(Run<'run, Text>) -> Run<'run, Text> + Send + Sync,
    {
        self.run_with_checkpoint(input, configure, &NoCheckpoint)
            .await
    }

    /// Record source decisions before beginning more work. The caller owns the
    /// storage transaction. Sources are recorded after independent re-gating, but
    /// can still include rejected decisions and require fresh validation on replay.
    pub async fn run_with_checkpoint<'run, F>(
        &'run self,
        input: AssembleInput,
        configure: F,
        checkpoint: &'run dyn AdjudicationCheckpoint,
    ) -> Result<AssembleOutput, Error>
    where
        'a: 'run,
        F: Fn(Run<'run, Text>) -> Run<'run, Text> + Send + Sync,
    {
        self.check_cancelled()?;
        let AssembleInput {
            graph,
            ontology,
            proposals,
            banked_adjudication,
            resolved_into,
            discarded_paths,
        } = input;
        ontology.load(&graph.scope_id).await?;
        let mut rows: BTreeMap<_, _> = graph
            .entities
            .iter()
            .cloned()
            .map(|row| (row.path.clone(), row))
            .collect();
        rows.extend(
            proposals
                .input_entities
                .iter()
                .map(|(path, row)| (path.clone(), row.clone())),
        );
        rows.extend(
            proposals
                .staged_entities
                .iter()
                .map(|(path, row)| (path.clone(), row.clone())),
        );
        for path in resolved_into.keys() {
            crate::view::resolve_path(path, |path| {
                Ok(resolved_into.get(path).map(String::as_str))
            })?;
        }
        let mut progress = Progress {
            banked: banked_adjudication,
            resolved_into,
            discarded_paths,
            rows,
        };
        let run = AssembleRun {
            checkpoint,
            harness: &self.harness,
            configure,
            gate: GateContext {
                graph: graph.clone(),
                prefixes: ontology.prefixes(),
                prompts: self.prompts.clone(),
            },
            cancellation: self.cancellation.clone(),
            max_submissions: self.max_submissions,
            max_tool_turns: self.max_tool_turns,
        };
        let entities = graph
            .entities
            .iter()
            .filter_map(KnowledgeConsolidationEntity::effective_entity)
            .collect::<Vec<_>>();
        let result = run
            .assemble(&ontology, &entities, &proposals, &mut progress)
            .await?;
        self.check_cancelled()?;
        Ok(result)
    }
    fn check_cancelled(&self) -> Result<(), Error> {
        if self.cancellation.is_cancelled() {
            Err(Error::Cancelled("assemble cancelled".into()))
        } else {
            Ok(())
        }
    }
}
impl Phase for Assemble<'_> {
    type Input = AssembleInput;
    type Output = AssembleOutput;
    async fn run(&self, input: Self::Input) -> Result<Self::Output, Error> {
        Assemble::run(self, input).await
    }
}
pub(super) struct AssembleRun<'a, 'run, F> {
    pub checkpoint: &'run dyn AdjudicationCheckpoint,
    pub harness: &'run Harness<'a>,
    pub configure: F,
    pub gate: GateContext,
    pub max_submissions: usize,
    pub max_tool_turns: Option<usize>,
    pub cancellation: tokio_util::sync::CancellationToken,
}
pub(super) struct Progress {
    banked: Option<AdjudicationResult>,
    resolved_into: BTreeMap<String, String>,
    discarded_paths: HashSet<String>,
    rows: BTreeMap<String, KnowledgeConsolidationEntity>,
}
impl Progress {
    pub fn adjudication(&self) -> Option<AdjudicationResult> {
        self.banked.clone()
    }
    pub fn bank_adjudication(&mut self, value: &AdjudicationResult) {
        let mut decisions: BTreeMap<_, _> = self
            .banked
            .iter()
            .flat_map(|held| held.decisions.iter().cloned())
            .map(|d| (d.term.clone(), d))
            .collect();
        decisions.extend(value.decisions.iter().cloned().map(|d| (d.term.clone(), d)));
        self.banked = Some(AdjudicationResult {
            decisions: decisions.into_values().collect(),
            amendment_nominations: value.amendment_nominations.clone(),
        });
    }
    pub fn resolved_into(&self) -> &BTreeMap<String, String> {
        &self.resolved_into
    }
    pub fn discarded_paths(&self) -> impl Iterator<Item = &String> {
        self.discarded_paths.iter()
    }
    pub fn entity_row(&self, path: &str) -> Option<&KnowledgeConsolidationEntity> {
        self.rows.get(path)
    }
    pub fn canonical_path(&self, path: &str) -> String {
        crate::view::resolve_path(path, |path| {
            Ok(self.resolved_into.get(path).map(String::as_str))
        })
        .expect("assemble validated redirect chains")
        .to_string()
    }
}

impl<'a, 'run, F> std::ops::Deref for AssembleRun<'a, 'run, F> {
    type Target = GateContext;
    fn deref(&self) -> &Self::Target {
        &self.gate
    }
}
