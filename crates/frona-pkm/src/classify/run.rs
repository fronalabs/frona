use super::{
    logic::{ClassificationInspection, ClassifyLogic},
    *,
};
use crate::prompt::{PromptIds, prompt_evidence};
use crate::{
    model::LinkOrigin,
    tools::ontology::{OntologyToolProfile, build_ontology_tools_with_overlay, overlay},
};
use frona_harness::{FailureKind, ManualError, RunOutcome};
use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicUsize},
};

pub(super) async fn run<'a, 'run, F>(
    phase: &'run Classify<'a>,
    mut input: ClassifyInput,
    configure: F,
    diagnostics: Option<&dyn ClassificationDiagnostics>,
) -> Result<ClassifyOutput, Error>
where
    'a: 'run,
    F: FnOnce(Run<'run, Text>) -> Run<'run, Text> + Send,
{
    if phase.max_submissions == 0 {
        return Err(Error::Internal(
            "classification: max_submissions must be at least one".into(),
        ));
    }
    let entity = input
        .graph
        .entity_by_path_with(&input.proposals.entity_draft(), &input.path)?
        .ok_or_else(|| Error::NotFound(format!("classification entity {}", input.path)))?;
    input.proposals.stage_input_entity(entity.clone());
    let ontology = &input.ontology;
    let graph = &input.graph;
    let mut facts = graph.current_memories_for_entity(&entity.path);
    for memory in graph.memories_by_ids(&entity.source_memory_ids) {
        if !facts.iter().any(|held| held.id == memory.id) {
            facts.push(memory);
        }
    }
    let prompt_ids = PromptIds::new("f", facts.iter().map(|m| m.id.clone()));
    let mut fact_lines = String::new();
    for memory in &facts {
        let mut entities = graph
            .memory_entities
            .get(&memory.id)
            .cloned()
            .ok_or_else(|| {
                Error::Internal(format!(
                    "classification snapshot missing memory scope for {}",
                    memory.id
                ))
            })?;
        entities.extend(input.proposals.memory_paths(&memory.id));
        entities.sort();
        entities.dedup();
        fact_lines.push_str(&format!(
            "- [{}] entities={} evidence={} {}\n",
            prompt_ids.local(&memory.id),
            serde_json::to_string(&entities).unwrap_or_else(|_| "[]".into()),
            prompt_evidence(&memory.evidence),
            memory.content
        ));
    }
    let catalogue = ontology.catalog(&graph.scope_id).await.unwrap_or_default();
    let minted = catalogue.classes.join(", ");
    let links: Vec<_> = graph
        .links
        .iter()
        .filter(|link| link.from_entity_path == entity.path)
        .cloned()
        .collect();
    let relation_lines: String = links
        .iter()
        .filter(|l| l.origin != LinkOrigin::Inferred)
        .map(|l| format!("- \"{}\" → {}\n", l.relation, l.to_entity_path))
        .collect();
    let logic = ClassifyLogic {
        retrieval: phase.retrieval.clone(),
        prefixes: ontology.prefixes(),
        prompts: phase.prompts.clone(),
        input: Arc::new(input.clone()),
    };
    let (attributes, evidence) = logic
        .classification_evidence(ontology, &entity, &links)
        .await;
    let evidence_text =
        serde_json::to_string_pretty(&evidence).map_err(|e| Error::Internal(e.to_string()))?;
    let contributions=serde_json::to_string_pretty(&entity.contributions.iter().map(|item|serde_json::json!({"name":item.name,"description":item.description,"aliases":item.aliases,"attributes":item.attributes,"existing_only":item.existing_only,"occurrence_count":item.occurrence_count})).collect::<Vec<_>>()).map_err(|e|Error::Internal(e.to_string()))?;
    let identity = prompt_evidence(&entity.identity_evidence);
    let vars = [
        ("name", entity.name.as_str()),
        ("description", or_none(&entity.description)),
        ("facts", or_none(&fact_lines)),
        ("relations", or_none(&relation_lines)),
        ("attributes", or_none(&attributes)),
        ("minted", &minted),
        ("evidence", &evidence_text),
        ("contributions", &contributions),
        ("identity_evidence", &identity),
    ];
    let (system, mut prompt) = phase.prompts.render(&vars)?;
    if let Some(rejection) = &input.initial_rejection {
        prompt.push_str(&format!("\n\nA previously accepted checkpoint proposal is no longer valid against the reconstructed graph. Revise it before it can be staged:\n{rejection}"));
    }
    let tool_overlay = overlay(
        ontology,
        graph,
        &input.proposals,
        phase.research_tool_budget,
    )?;
    let tools = build_ontology_tools_with_overlay(
        ontology.clone(),
        graph,
        logic.prefixes.clone(),
        Some(tool_overlay.clone()),
        OntologyToolProfile::Classify,
    )?;
    let existing: HashSet<_> = catalogue
        .classes
        .into_iter()
        .chain(catalogue.object_properties)
        .chain(catalogue.data_properties)
        .collect();
    let inspection = Arc::new(ClassificationInspection {
        logic: logic.clone(),
        ontology: ontology.clone(),
        entity: entity.clone(),
        proposals: input.proposals.clone(),
        prompt_ids: prompt_ids.clone(),
        existing,
        overlay: tool_overlay,
        requires_page_shape: !entity.contributions.is_empty(),
        attempts: AtomicUsize::new(0),
        failures: AtomicUsize::new(0),
        details: Default::default(),
        challenged: AtomicBool::new(false),
    });
    let run = configure(phase.harness.run(prompt).instructions(system).tools(tools))
        .exploration_turns(phase.max_tool_turns)
        .structured::<Classification>()
        .submission_attempts(phase.max_submissions);
    let report = run.report();
    let inspector = inspection.clone();
    let refined = run
        .inspect(move |candidate| {
            let inspector = inspector.clone();
            async move { inspector.inspect(candidate).await }
        })
        .await;
    let request_count = report.snapshot().logical_calls();
    let diagnostic = serde_json::json!({"evidence":evidence,"request_count":request_count,"one_request":request_count==1,"fallback_investigation_requests":request_count.saturating_sub(1),"validation":{"attempts":inspection.attempts.load(Ordering::Relaxed),"failures":inspection.failures.load(Ordering::Relaxed),"failure_batches":inspection.details.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()}});
    if let Some(diagnostics) = diagnostics {
        diagnostics.record(&diagnostic).await?;
    }
    let mut output = ClassifyOutput {
        classification: None,
        proposals: input.proposals,
        minted_paths: Vec::new(),
        diagnostic,
    };
    match refined {
        Err(incomplete) => match incomplete.reason {
            ManualError::Abandoned(_) => Ok(output),
            ManualError::SubmissionAttemptsExhausted { .. }
                if inspection.attempts.load(Ordering::Relaxed) > 0 =>
            {
                Ok(output)
            }
            reason
                if matches!(
                    incomplete.report.outcome,
                    Some(RunOutcome::Incomplete {
                        kind: FailureKind::Cancelled,
                        ..
                    })
                ) =>
            {
                Err(Error::Cancelled(reason.to_string()))
            }
            reason => Err(Error::Internal(format!(
                "classification incomplete: {reason}"
            ))),
        },
        Ok(mut candidate) => {
            candidate.expand_prompt_ids(&prompt_ids)?;
            let candidate = candidate.repaired(&logic.prefixes);
            logic
                .record_entity_proposal(
                    &input.ontology,
                    &entity,
                    &candidate,
                    &mut output.proposals,
                    &mut output.minted_paths,
                )
                .await;
            output.classification = Some(candidate);
            Ok(output)
        }
    }
}
fn or_none(value: &str) -> &str {
    if value.is_empty() { "(none)\n" } else { value }
}
