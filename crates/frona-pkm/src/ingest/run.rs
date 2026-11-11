use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use tracing::warn;

use crate::ingest::{
    IngestBatch, IngestInput, IngestPrompts, PendingEntity, PendingEntityUpdate, PendingMemory,
    PendingPlaybookCandidate,
};
use crate::ingest::{
    cleanup::{
        candidate_attribute_map, remove_multi_entity_candidate_attributes,
        terminal_cleanup_with_recall,
    },
    correction::{
        AgentEvidenceMetrics, GroundingCorrectionState, GroundingFeedback,
        agent_contribution_count, apply_allowed_revision, canonical_failure_state,
        contribution_count, correction_memory_ids, count_tool_supports, render_memory_evidence,
        validate_entity_revision_identity,
    },
    evidence::{
        batch_without_failed_contributions, candidate_attribute_evidence,
        is_agent_evidence_failure, resolve_evidence, resolve_evidence_with_tools,
    },
    prepare::prepare_ingest_batch,
    submission::Batch,
    temporal::{extraction_submission_limit, resolve_episode},
    validation::{
        distinct_assertion_message_count, research_coverage_stats, research_message_handles,
        validate_erroneous_memories, validate_extract_submission,
    },
};
use crate::model::{Episode, MemoryKind};
use crate::path::normalize_path;
use crate::{Error, TranscriptEvidenceKind};
use frona_harness::{Decision, ExecutionStop, FailureKind, ManualError, ReportHandle, RunOutcome};
use frona_harness::{Harness, Run, Text};

/// Mine one transcript into memories and provisional, untyped mention entities.
/// Failure leaves the transcript watermark unchanged so the pass can retry it.
pub(super) struct IngestRun<'a> {
    pub input: Arc<IngestInput>,
    pub prompts: IngestPrompts,
    pub harness: &'a Harness<'a>,
}

/// One run's domain state, shared with its tools and terminal cleanup. Each
/// inspector future owns an Arc so it remains Send across the server's task
/// boundaries without requiring borrowed inspector state to be 'static.
struct GroundingInspection {
    input: Arc<IngestInput>,
    prompts: IngestPrompts,
    failure_state: Mutex<GroundingCorrectionState>,
    reported_failures: Mutex<GroundingFeedback>,
    corrections: AtomicUsize,
    no_tool_drops: AtomicUsize,
    no_tool_items: Mutex<HashSet<String>>,
    strong_matches: AtomicUsize,
    fallback_reviews: AtomicUsize,
    invalid_submissions: AtomicUsize,
    citation_repairs: AtomicUsize,
    coverage_memory_additions: AtomicUsize,
    mixed_claim_splits: AtomicUsize,
    searched_messages: Arc<Mutex<HashSet<String>>>,
    research_messages: Arc<HashSet<String>>,
    report: ReportHandle,
}

impl GroundingInspection {
    async fn inspect(
        &self,
        mut batch: Batch,
        max_submissions: usize,
    ) -> Result<Decision<Batch>, Error> {
        let input = &self.input;
        let state = &self.failure_state;
        let reported_ref = &self.reported_failures;
        let corrections_ref = &self.corrections;
        let searched_ref = &self.searched_messages;
        let research_ref = &self.research_messages;
        let no_tool_ref = &self.no_tool_drops;
        let no_tool_items_ref = &self.no_tool_items;
        let strong_ref = &self.strong_matches;
        let fallback_ref = &self.fallback_reviews;
        let invalid_ref = &self.invalid_submissions;
        let citation_ref = &self.citation_repairs;
        let additions_ref = &self.coverage_memory_additions;
        let splits_ref = &self.mixed_claim_splits;
        let report = &self.report;
        let (revision, revision_failures) = {
            let state = state.lock().expect("grounding failure state poisoned");
            state
                .retained
                .as_ref()
                .map(|previous| {
                    let revision_failures =
                        validate_entity_revision_identity(previous, &batch, &state.failures);
                    let failed_memories = state
                        .failures
                        .iter()
                        .filter_map(|failure| {
                            failure
                                .field_path
                                .strip_prefix("memories[")?
                                .split_once(']')?
                                .0
                                .parse::<usize>()
                                .ok()
                        })
                        .collect::<BTreeSet<_>>();
                    let coverage_repair = state
                        .failures
                        .iter()
                        .any(|failure| failure.reason == "research_message_unaccounted");
                    let merged = apply_allowed_revision(previous, &batch, &state.failures);
                    if coverage_repair {
                        additions_ref.fetch_add(
                            merged
                                .memories
                                .len()
                                .saturating_sub(previous.memories.len()),
                            Ordering::Relaxed,
                        );
                    }
                    if !failed_memories.is_empty() {
                        let retained = previous
                            .memories
                            .len()
                            .saturating_sub(failed_memories.len());
                        splits_ref.fetch_add(
                            merged
                                .memories
                                .len()
                                .saturating_sub(retained)
                                .saturating_sub(failed_memories.len()),
                            Ordering::Relaxed,
                        );
                    }
                    (merged, revision_failures)
                })
                .map_or((None, Vec::new()), |(merged, failures)| {
                    (Some(merged), failures)
                })
        };
        if let Some(merged) = revision {
            batch = merged;
        }
        let supports_before = count_tool_supports(&batch);
        let mut failures = {
            let searched = searched_ref
                .lock()
                .expect("searched evidence messages poisoned");
            validate_extract_submission(
                &mut batch,
                &input.evidence_sources,
                &input.temporal_sources,
                &input.recall,
                &searched,
                research_ref,
                citation_ref,
            )
        };
        failures.extend(validate_erroneous_memories(&batch, input));
        failures.extend(revision_failures);
        let newly_rejected_without_tool = {
            let mut seen = no_tool_items_ref
                .lock()
                .expect("no-tool evidence state poisoned");
            failures
                .iter()
                .filter(|failure| failure.reason == "agent_claim_without_tool_evidence")
                .filter(|failure| seen.insert(failure.fingerprint()))
                .count()
        };
        no_tool_ref.fetch_add(newly_rejected_without_tool, Ordering::Relaxed);
        strong_ref.fetch_add(
            count_tool_supports(&batch).saturating_sub(supports_before),
            Ordering::Relaxed,
        );
        fallback_ref.fetch_add(
            failures
                .iter()
                .filter(|failure| failure.reason == "agent_claim_needs_tool_evidence")
                .count(),
            Ordering::Relaxed,
        );
        invalid_ref.fetch_add(
            failures
                .iter()
                .filter(|failure| {
                    matches!(
                        failure.reason,
                        "tool_evidence_without_agent_source"
                            | "unknown_tool_evidence"
                            | "tool_evidence_quote_not_found"
                            | "tool_evidence_clause_mismatch"
                            | "selected_evidence_missing_critical_values"
                    )
                })
                .count(),
            Ordering::Relaxed,
        );
        if failures.is_empty() {
            // An empty result trivially has nothing unsupported. Accepting it
            // would let a model escape the check by giving up, discarding an
            // earlier round that did find grounded content.
            let accepted_recall_omission = {
                let state = state.lock().expect("grounding failure state poisoned");
                state.retained.as_ref().is_some_and(|previous| {
                    state
                        .failures
                        .iter()
                        .any(|failure| is_agent_evidence_failure(failure.reason))
                        && serde_json::to_value(batch_without_failed_contributions(
                            previous,
                            &state.failures,
                        ))
                        .ok()
                            == serde_json::to_value(&batch).ok()
                })
            };
            return Ok(
                if batch.new_entities.is_empty()
                    && batch.existing_entity_updates.is_empty()
                    && batch.playbooks.is_empty()
                    && batch.memories.is_empty()
                    && batch.research_dispositions.is_empty()
                    && !accepted_recall_omission
                {
                    Decision::abandon("extract returned no contributions")
                } else {
                    Decision::accept(batch)
                },
            );
        }
        let fingerprints = canonical_failure_state(&failures);
        let streak = {
            let mut state = state.lock().expect("grounding failure state poisoned");
            if state.fingerprints == fingerprints {
                state.streak += 1
            } else {
                state.fingerprints = fingerprints.clone();
                state.streak = 1;
            }
            state.retained = Some(batch.clone());
            state.failures = failures.clone();
            state.streak
        };
        let submission = corrections_ref.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            submission, max_submissions, failures = failures.len(), streak,
            fingerprints = ?fingerprints, "pkm extract grounding rejected"
        );
        if submission >= max_submissions {
            let reported = reported_ref
                .lock()
                .expect("reported grounding state poisoned");
            if reported.has_unreported(&fingerprints, &report.snapshot()) {
                return Err(Error::Internal(
                    "extract correction budget ended with a validation error that was not sent to the model; the window will retry without advancing its watermark".into(),
                ));
            }
            warn!(
                unsupported = failures.len(),
                submissions = submission,
                "extract: grounding budget exhausted; applying terminal cleanup"
            );
            let retained = state
                .lock()
                .expect("grounding failure state poisoned")
                .retained
                .clone()
                .unwrap_or(batch);
            return Ok(Decision::stop("extract correction budget exhausted").retain(retained));
        }
        let rejected = failures
            .iter()
            .map(|failure| failure.render_with_allowed(None))
            .collect::<Vec<_>>();
        let (accepted_memories, memory_repairs) = correction_memory_ids(&batch, &failures);
        let accepted_memories = render_memory_evidence(&batch, &accepted_memories);
        let memory_repairs = render_memory_evidence(&batch, &memory_repairs);
        let feedback = self.prompts.reject(&[
            ("rejected", &rejected.join("\n")),
            ("accepted_memories", &accepted_memories),
            ("memory_repairs", &memory_repairs),
        ])?;
        reported_ref
            .lock()
            .expect("reported grounding state poisoned")
            .queue(fingerprints, &report.snapshot())?;
        let keep = state
            .lock()
            .expect("grounding failure state poisoned")
            .retained
            .clone()
            .unwrap_or(batch);
        Ok(Decision::revise(feedback).retain(keep))
    }
}

impl<'a> IngestRun<'a> {
    pub async fn run<F>(&self, configure: F) -> Result<IngestBatch, Error>
    where
        F: FnOnce(Run<'a, Text>) -> Run<'a, Text> + Send,
    {
        let mut batch = self
            .extract(&self.input.transcript, configure)
            .await?
            .unwrap_or_default();
        prepare_ingest_batch(&mut batch);
        Ok(batch)
    }

    /// Refine submissions against evidence, preserving accepted contributions across
    /// corrections. Retained candidates remain unaccepted SDK results; domain cleanup
    /// may salvage only independently validated contributions. Cancellation and
    /// inspector errors always propagate so the host can retry the window.
    async fn extract_grounded<F>(
        &self,
        vars: &[(&str, &str)],
        configure: F,
    ) -> Result<(Batch, usize, usize, usize, AgentEvidenceMetrics), Error>
    where
        F: FnOnce(Run<'a, Text>) -> Run<'a, Text> + Send,
    {
        let transcript_turns = self
            .input
            .evidence_sources
            .iter()
            .filter(|source| {
                matches!(
                    source.kind,
                    TranscriptEvidenceKind::UserMessage { .. }
                        | TranscriptEvidenceKind::AgentMessage { .. }
                )
            })
            .count();
        let max_submissions = extraction_submission_limit(transcript_turns);
        let max_tool_turns = max_submissions;
        let recall_lookups = Arc::new(AtomicUsize::new(0));
        let evidence_lookups = Arc::new(AtomicUsize::new(0));
        let searched_messages = Arc::new(Mutex::new(HashSet::new()));
        let evidence_assertions = self
            .input
            .evidence_sources
            .iter()
            .filter_map(|source| {
                let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &source.kind else {
                    return None;
                };
                Some((source.handle.clone(), message_id.clone()))
            })
            .collect::<HashMap<_, _>>();
        let research_messages = Arc::new(research_message_handles(
            &self.input.evidence_sources,
            &self.input.recall.evidence,
        ));
        let tools = super::tools::registry(
            self.input.recall.clone(),
            recall_lookups.clone(),
            evidence_lookups.clone(),
            evidence_assertions,
            max_submissions,
            searched_messages.clone(),
        )?;
        let (system, input) = self.prompts.render(vars)?;
        let run = configure(self.harness.run(input).instructions(system).tools(tools))
            .exploration_turns(max_tool_turns)
            .structured::<Batch>()
            .submission_attempts(max_submissions);
        let report = run.report();

        let inspection = Arc::new(GroundingInspection {
            input: self.input.clone(),
            prompts: self.prompts.clone(),
            failure_state: Mutex::default(),
            reported_failures: Mutex::default(),
            corrections: AtomicUsize::default(),
            no_tool_drops: AtomicUsize::default(),
            no_tool_items: Mutex::default(),
            strong_matches: AtomicUsize::default(),
            fallback_reviews: AtomicUsize::default(),
            invalid_submissions: AtomicUsize::default(),
            citation_repairs: AtomicUsize::default(),
            coverage_memory_additions: AtomicUsize::default(),
            mixed_claim_splits: AtomicUsize::default(),
            searched_messages: searched_messages.clone(),
            research_messages: research_messages.clone(),
            report: report.clone(),
        });
        // The transcript is consumed on Ok, so a missing first candidate must
        // propagate from the incomplete branch rather than advance the watermark.
        let inspector_state = inspection.clone();
        let refined = run
            .inspect(move |batch| {
                let inspection = inspector_state.clone();
                async move { inspection.inspect(batch, max_submissions).await }
            })
            .await;

        // Retention never becomes SDK acceptance. Only this application branch
        // may salvage incomplete work, through the independent validation below.
        // Cancellation and inspector failures must keep the window retryable.
        let mut batch = match refined {
            Ok(accepted) => accepted,
            Err(incomplete) => {
                warn!(reason = %incomplete.reason, outcome = ?incomplete.report.outcome,
                    "extract: refinement incomplete");
                if matches!(
                    incomplete.report.outcome,
                    Some(RunOutcome::Incomplete {
                        kind: FailureKind::Cancelled,
                        ..
                    })
                ) {
                    return Err(Error::Cancelled(incomplete.reason.to_string()));
                }
                match incomplete.reason {
                    ManualError::Inspector(reason) => return Err(Error::Internal(reason)),
                    ManualError::Abandoned(_) if incomplete.retained.is_none() => Batch {
                        new_entities: Vec::new(),
                        existing_entity_updates: Vec::new(),
                        playbooks: Vec::new(),
                        memories: Vec::new(),
                        research_dispositions: Vec::new(),
                    },
                    reason @ (ManualError::Abandoned(_)
                    | ManualError::SubmissionAttemptsExhausted { .. }
                    | ManualError::Model(_)
                    | ManualError::Stop(
                        ExecutionStop::Stopped(_) | ExecutionStop::BudgetExhausted(_),
                    )) => incomplete.retained.ok_or_else(|| {
                        Error::Internal(format!("extract ended without a candidate: {reason}"))
                    })?,
                    reason => return Err(Error::Internal(format!("extract failed: {reason}"))),
                }
            }
        };
        let agent_before_cleanup = agent_contribution_count(&batch, &self.input.evidence_sources);
        let mut terminal_failures = {
            let searched = searched_messages
                .lock()
                .expect("searched evidence messages poisoned");
            validate_extract_submission(
                &mut batch,
                &self.input.evidence_sources,
                &self.input.temporal_sources,
                &self.input.recall,
                &searched,
                &research_messages,
                &inspection.citation_repairs,
            )
        };
        terminal_failures.extend(validate_erroneous_memories(&batch, &self.input));
        let has_unreported_terminal_failure = {
            let reported = inspection
                .reported_failures
                .lock()
                .expect("reported grounding state poisoned");
            reported.has_unreported(
                &canonical_failure_state(&terminal_failures),
                &report.snapshot(),
            )
        };
        if has_unreported_terminal_failure {
            return Err(Error::Internal(
                "extract terminal cleanup found a validation error that was not sent to the model; the window will retry without advancing its watermark".into(),
            ));
        }
        let newly_rejected_without_tool = {
            let mut seen = inspection
                .no_tool_items
                .lock()
                .expect("no-tool evidence state poisoned");
            terminal_failures
                .iter()
                .filter(|failure| failure.reason == "agent_claim_without_tool_evidence")
                .filter(|failure| seen.insert(failure.fingerprint()))
                .count()
        };
        inspection
            .no_tool_drops
            .fetch_add(newly_rejected_without_tool, Ordering::Relaxed);
        let before_failed_cleanup = contribution_count(&batch);
        batch = batch_without_failed_contributions(&batch, &terminal_failures);
        let failed_cleanup_drops = before_failed_cleanup.saturating_sub(contribution_count(&batch));
        let dropped = failed_cleanup_drops
            + terminal_cleanup_with_recall(
                &mut batch,
                &self.input.evidence_sources,
                &self.input.recall,
            );
        for memory in &batch.memories {
            let recall_ids = memory
                .sources
                .iter()
                .filter_map(|citation| {
                    let source = self
                        .input
                        .evidence_sources
                        .iter()
                        .find(|source| source.handle == citation.message)?;
                    let TranscriptEvidenceKind::AgentMessage { message_id, .. } = &source.kind
                    else {
                        return None;
                    };
                    Some(
                        self.input
                            .recall
                            .result_calls_for_message(message_id)
                            .iter()
                            .map(|call| call.local_id.as_str())
                            .collect::<Vec<_>>(),
                    )
                })
                .flatten()
                .collect::<Vec<_>>();
            if !recall_ids.is_empty() {
                tracing::info!(
                    memory = %memory.content,
                    recall_calls = ?recall_ids,
                    reason = "no attached recall result supported the complete retained claim",
                    "pkm extract: retained Agent-sourced candidate after recall check"
                );
            }
        }
        let strong = inspection.strong_matches.load(Ordering::Relaxed);
        let total_supports = count_tool_supports(&batch);
        let evidence_terminal_drops = agent_before_cleanup.saturating_sub(
            agent_contribution_count(&batch, &self.input.evidence_sources),
        );
        let mut research_coverage =
            research_coverage_stats(&batch, &self.input.evidence_sources, &research_messages);
        research_coverage.memories_added_by_repair =
            inspection.coverage_memory_additions.load(Ordering::Relaxed);
        research_coverage.citation_repairs = inspection.citation_repairs.load(Ordering::Relaxed);
        research_coverage.mixed_claim_splits =
            inspection.mixed_claim_splits.load(Ordering::Relaxed);
        Ok((
            batch,
            inspection.corrections.load(Ordering::Relaxed),
            dropped,
            recall_lookups.load(Ordering::Relaxed),
            AgentEvidenceMetrics {
                no_tool_drops: inspection.no_tool_drops.load(Ordering::Relaxed),
                strong_matches: strong,
                fallback_reviews: inspection.fallback_reviews.load(Ordering::Relaxed),
                fallback_retains: total_supports.saturating_sub(strong),
                invalid_submissions: inspection.invalid_submissions.load(Ordering::Relaxed),
                lookup_calls: evidence_lookups.load(Ordering::Relaxed),
                terminal_drops: evidence_terminal_drops,
                research_coverage,
            },
        ))
    }

    /// Convert grounded submissions using the caller's snapshot. Persistence belongs
    /// to the caller, including the atomic batch/watermark commit.
    async fn extract<F>(&self, transcript: &str, configure: F) -> Result<Option<IngestBatch>, Error>
    where
        F: FnOnce(Run<'a, Text>) -> Run<'a, Text> + Send,
    {
        if transcript.trim().is_empty() {
            return Ok(None);
        }

        let self_path = &self.input.self_path;
        let owner_name = if self.input.owner_name.trim().is_empty() {
            "the account owner"
        } else {
            self.input.owner_name.as_str()
        };

        // A failed render is an `Err` (see `IngestPrompts`), so the empty-prompt hazard is
        // handled for every stage now - but keep the transcript check below: it is about
        // *content*, not templating. A template that renders fine while dropping
        // `{{transcript}}` would still hand the model nothing to answer from.
        let mut existing_pages = self.input.existing_entities.clone();
        existing_pages.sort_by(|a, b| a.path.cmp(&b.path));
        let existing_entities = existing_pages
            .iter()
            .map(|entity| format!("- `{}` — {}", entity.path, entity.name))
            .collect::<Vec<_>>()
            .join("\n");
        let existing_entities = if existing_entities.is_empty() {
            "(none)".to_string()
        } else {
            existing_entities
        };
        let mut research_messages =
            research_message_handles(&self.input.evidence_sources, &self.input.recall.evidence)
                .into_iter()
                .collect::<Vec<_>>();
        research_messages.sort();
        let research_messages = if research_messages.is_empty() {
            "(none)".to_string()
        } else {
            research_messages
                .into_iter()
                .map(|message| format!("- `{message}`"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let vars = [
            ("owner_name", owner_name),
            ("handle", self.input.handle.as_str()),
            ("self_path", self_path),
            ("existing_entities", &existing_entities),
            ("research_messages", &research_messages),
            ("transcript", transcript),
        ];

        // A failure must propagate so the sweep does not advance the watermark.
        let (
            mut parsed,
            grounding_corrections,
            grounding_items_dropped,
            recall_result_lookups,
            agent_evidence,
        ) = self.extract_grounded(&vars, configure).await?;
        remove_multi_entity_candidate_attributes(&mut parsed);

        let mut out = IngestBatch {
            grounding_corrections,
            grounding_items_dropped,
            recall_result_lookups,
            agent_evidence_no_tool_drops: agent_evidence.no_tool_drops,
            agent_evidence_strong_matches: agent_evidence.strong_matches,
            agent_evidence_fallback_reviews: agent_evidence.fallback_reviews,
            agent_evidence_fallback_retains: agent_evidence.fallback_retains,
            agent_evidence_invalid_submissions: agent_evidence.invalid_submissions,
            agent_evidence_lookup_calls: agent_evidence.lookup_calls,
            agent_evidence_terminal_drops: agent_evidence.terminal_drops,
            research_coverage: agent_evidence.research_coverage,
            ..Default::default()
        };
        for entity in &parsed.new_entities {
            let Some(path) = normalize_path(&entity.path) else {
                // Only reachable for a proposed path with no alphanumeric content at all
                // once transliterated - punctuation, or symbols with no ASCII rendering.
                // Logged rather than skipped in silence: the transcript is consumed
                // once, so an entity dropped here is dropped for good.
                warn!(
                    proposed = %entity.path,
                    entity = %entity.name,
                    "pkm extract: proposed path has no usable segment, entity dropped"
                );
                continue;
            };
            // Attributes are the extractor's ONLY structured output about an entity, and
            // they stay free-text keyed: naming them against a vocabulary, and deciding
            // which of them are really edges to another entity needs the catalogue. Ingest
            // sees existing entity names only to reuse their identity, and has no ontology
            // vocabulary. The Classify stage decides a stage later - see `classify`'s
            // `AttributeMapping`.
            //
            // Carried as a map rather than flattened into `"{key}: {value}"` strings so
            // that decision has structure to work from instead of prose to re-parse.
            let attribute_evidence = candidate_attribute_evidence(
                &entity.candidate_attributes,
                &self.input.evidence_sources,
                &self.input.recall.evidence,
            );
            let mut attributes = candidate_attribute_map(&entity.candidate_attributes);
            attributes.retain(|key, _| attribute_evidence.contains_key(key));
            out.entities.push(PendingEntity {
                path,
                name: entity.name.clone(),
                description: entity.description.clone(),
                aliases: entity.aliases.clone(),
                identity_evidence: resolve_evidence(&entity.sources, &self.input.evidence_sources)
                    .unwrap_or_default(),
                attributes: serde_json::Value::Object(attributes),
                attribute_evidence,
            });
        }

        let existing_paths: HashSet<&str> = existing_pages
            .iter()
            .map(|entity| entity.path.as_str())
            .collect();
        for update in &parsed.existing_entity_updates {
            let Some(path) = normalize_path(&update.path) else {
                continue;
            };
            if !existing_paths.contains(path.as_str()) {
                warn!(
                    proposed = %update.path,
                    "pkm extract: existing-entity update names an entity absent from the input"
                );
                continue;
            }
            let attribute_evidence = candidate_attribute_evidence(
                &update.candidate_attributes,
                &self.input.evidence_sources,
                &self.input.recall.evidence,
            );
            let mut attributes = candidate_attribute_map(&update.candidate_attributes);
            attributes.retain(|key, _| attribute_evidence.contains_key(key));
            if attributes.is_empty() {
                continue;
            }
            out.entity_updates.push(PendingEntityUpdate {
                path,
                attributes: serde_json::Value::Object(attributes),
                attribute_evidence,
            });
        }

        // Memories keep mention paths until a later phase resolves identity. The
        // caller's rejected-content snapshot prevents relearning retired facts.
        let mut playbook_by_local: HashMap<String, usize> = HashMap::new();
        for candidate in parsed.playbooks {
            let local = candidate.id.trim();
            let name = candidate.name.trim();
            let description = candidate.description.trim();
            let path = normalize_path(&candidate.path).ok_or_else(|| {
                Error::Internal(
                    "extract accepted a playbook candidate with an unusable path".into(),
                )
            })?;
            if local.is_empty()
                || name.is_empty()
                || description.is_empty()
                || playbook_by_local.contains_key(local)
            {
                return Err(Error::Internal(
                    "extract accepted an invalid or duplicate playbook candidate".into(),
                ));
            }
            let index = out.playbook_candidates.len();
            out.playbook_candidates.push(PendingPlaybookCandidate {
                id: uuid::Uuid::now_v7().to_string(),
                path,
                name: name.to_string(),
                description: description.to_string(),
                source_memory_ids: Default::default(),
            });
            playbook_by_local.insert(local.to_string(), index);
        }

        for mem in parsed.memories {
            let kind = MemoryKind::parse(&mem.kind).ok_or_else(|| {
                Error::Internal("extract accepted a memory with an invalid kind".into())
            })?;
            let playbook_index = match (kind == MemoryKind::Procedural, mem.playbook.as_deref()) {
                (true, Some(local)) => playbook_by_local.get(local.trim()).copied(),
                (true, None) | (false, Some(_)) => None,
                (false, None) => Some(usize::MAX),
            };
            if playbook_index.is_none() {
                return Err(Error::Internal(
                    "extract accepted a memory with an invalid procedural playbook reference"
                        .into(),
                ));
            }
            if (kind == MemoryKind::Episodic) != mem.episode.is_some() {
                return Err(Error::Internal(
                    "extract accepted a memory with invalid episodic metadata".into(),
                ));
            }
            let mut episode = mem.episode.map(|episode| Episode {
                status: episode.status,
                anchor: episode.anchor,
                duration: episode.duration,
                absolute: episode.absolute,
                resolved_start: None,
                resolved_end: None,
            });
            if episode
                .as_ref()
                .is_some_and(|e| e.duration.is_some() && e.absolute.is_some())
            {
                return Err(Error::Internal(
                    "extract accepted an episode with both relative and absolute time".into(),
                ));
            }
            if let Some(episode) = &mut episode {
                resolve_episode(episode, &self.input.temporal_sources, &self.input.timezone);
            }
            let content = mem.content.trim();
            if content.is_empty() {
                return Err(Error::Internal(
                    "extract accepted a memory with empty content".into(),
                ));
            }
            let paths: Vec<String> = mem
                .entities
                .iter()
                .filter_map(|p| normalize_path(p))
                .collect();
            if paths.is_empty() {
                return Err(Error::Internal(
                    "extract accepted a memory without a usable entity".into(),
                ));
            }
            let Some(evidence) = resolve_evidence_with_tools(
                &mem.sources,
                &mem.tool_evidence,
                &self.input.evidence_sources,
                &self.input.recall.evidence,
            ) else {
                warn!(
                    memory = %content,
                    "pkm extract: accepted memory had unresolvable evidence; memory dropped"
                );
                out.grounding_items_dropped += 1;
                continue;
            };
            if kind == MemoryKind::Procedural && distinct_assertion_message_count(&evidence) != 1 {
                return Err(Error::Internal(
                    "extract accepted a Procedural memory without exactly one User or Agent assertion source".into(),
                ));
            }
            let memory_id = uuid::Uuid::now_v7().to_string();
            out.memories.push(PendingMemory {
                id: memory_id.clone(),
                kind,
                evidence,
                episode,
                content: content.to_string(),
                paths,
            });
            if let Some(index) = playbook_index.filter(|index| *index != usize::MAX) {
                out.playbook_candidates[index]
                    .source_memory_ids
                    .insert(memory_id);
            }
        }
        out.playbook_candidates
            .retain(|candidate| !candidate.source_memory_ids.is_empty());
        Ok(Some(out))
    }
}
