use super::*;

impl PkmService {
    pub fn new(
        db: Surreal<Db>,
        storage_service: StorageService,
        registry: Arc<ModelProviderRegistry>,
        prompts: PromptLoader,
        memory_config: MemoryConfig,
        user_service: crate::auth::user_service::UserService,
        ontology_roots: ontology::Roots,
    ) -> Self {
        let repo = Arc::new(PkmRepo::new(db.clone(), memory_config.pkm_search_top_k));
        // The Ontology Memory engine shares the service's `PkmRepo`, so it's built
        // here rather than attached after the fact. It loads a catalogue if one is
        // already on disk and starts without one otherwise - the release is downloaded,
        // so its absence is a normal state on a fresh install, not a failure.
        let ontology_manager = ontology::OntologyManager::new(ontology_roots, repo.clone());
        let operations = operations::PkmOperationCoordinator::default();
        Self {
            repo,
            tool_calls: Arc::new(SurrealToolCallRepo::new(db.clone())),
            messages: crate::db::repo::messages::SurrealMessageRepo::new(db.clone()),
            storage: PkmStorage::new(storage_service),
            registry,
            prompts,
            memory_config,
            user_service,
            ontology_manager,
            operations,
            reset_state: reset::PkmResetStateStore::new(
                crate::core::runtime_config::RuntimeConfigStore::new(db),
            ),
        }
    }

    /// The DB handle this service reads and writes the knowledge base through.
    ///
    /// `PkmSyncService` must use the same repository so rename and CAS state stay
    /// consistent between the services.
    pub fn repo(&self) -> Arc<PkmRepo> {
        self.repo.clone()
    }

    /// The filesystem projection this service writes. Shared with `PkmSyncService` for
    /// the same reason as [`repo`](Self::repo).
    pub fn storage(&self) -> PkmStorage {
        self.storage.clone()
    }

    pub fn ontology_manager(&self) -> ontology::OntologyManager {
        self.ontology_manager.clone()
    }

    pub(crate) fn operation_coordinator(&self) -> operations::PkmOperationCoordinator {
        self.operations.clone()
    }

    fn detached_context(
        &self,
        scope: ConsolidationScope,
        harness: Arc<crate::agent::harness::Harness>,
    ) -> Result<Arc<ConsolidationContext>, AppError> {
        self.detached_context_with_cancel(
            scope,
            harness,
            tokio_util::sync::CancellationToken::new(),
        )
    }

    fn detached_context_with_cancel(
        &self,
        scope: ConsolidationScope,
        harness: Arc<crate::agent::harness::Harness>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<Arc<ConsolidationContext>, AppError> {
        let llm = ConsolidationInference::with_cancel_token(
            harness,
            self.consolidation_model_group()?,
            self.prompts.clone(),
            scope.user_id.clone(),
            cancel_token,
        );
        Ok(Arc::new(ConsolidationContext::detached(
            scope,
            self.repo.clone(),
            self.storage.clone(),
            llm,
        )))
    }

    pub async fn mine_window(
        &self,
        scope: ConsolidationScope,
        transcript: &str,
        harness: Arc<crate::agent::harness::Harness>,
    ) -> Result<crate::db::repo::pkm::IngestBatch, AppError> {
        let ctx = self.detached_context(scope, harness)?;
        Ingest::new(ctx).run(transcript).await
    }

    pub(super) async fn mine_window_with_cancel(
        &self,
        scope: ConsolidationScope,
        transcript: &str,
        harness: Arc<crate::agent::harness::Harness>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<crate::db::repo::pkm::IngestBatch, AppError> {
        let ctx = self.detached_context_with_cancel(scope, harness, cancel_token)?;
        Ingest::new(ctx).run(transcript).await
    }

    /// Re-author one existing concept page without running the consolidation state machine.
    /// Intended for focused maintenance and prompt iteration; it uses the production Page
    /// Author and removes the temporary effective-state overlay before returning.
    pub async fn author_page(
        &self,
        scope: ConsolidationScope,
        path: &str,
        harness: Arc<crate::agent::harness::Harness>,
    ) -> Result<bool, AppError> {
        let ctx = self.detached_context(scope, harness)?;
        let consolidation_id = ctx.view.consolidation_id().to_string();
        let outcome = consolidation::PageAuthor {
            ctx,
            prefixes: self.ontology_manager.prefixes(),
            concurrency: 1,
        }
        .run(&[path.to_string()])
        .await;
        self.repo
            .delete_consolidation_entities(&consolidation_id)
            .await?;
        Ok(outcome.pages_built == 1)
    }

    /// **Per user, once** - type, reconcile, author, and GC whatever the ingests
    /// left dirty.
    ///
    pub async fn consolidate(
        &self,
        scope: ConsolidationScope,
        harness: Arc<crate::agent::harness::Harness>,
    ) -> Result<ConsolidationStats, AppError> {
        self.consolidate_with_cancel(scope, harness, tokio_util::sync::CancellationToken::new())
            .await
    }

    pub(super) async fn consolidate_with_cancel(
        &self,
        scope: ConsolidationScope,
        harness: Arc<crate::agent::harness::Harness>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Result<ConsolidationStats, AppError> {
        let _execution = harness.execution_registry.start(
            &scope.user_id,
            crate::core::execution::NewExecution {
                title: "Memory consolidation".to_string(),
                kind: crate::core::execution::ExecutionKind::Memory,
                action: Some("Consolidating memory".to_string()),
                source: Some(crate::core::execution::ExecutionSource {
                    kind: crate::core::execution::ExecutionSourceKind::System,
                    id: None,
                }),
                related_chat_ids: Vec::new(),
                can_cancel: false,
            },
        );
        // Mining's counts are already banked: the sweep merges them into the record and
        // saves it before this opens it. Merging them again here would double every
        // extract count in the pass log.
        let record = self.open_record(&scope.user_id).await?;
        let ctx = self.context(
            scope,
            harness,
            self.consolidation_model_group()?,
            record,
            cancel_token,
        );
        let consolidator = Consolidator::new(
            ctx.clone(),
            self.memory_config.clone(),
            self.ontology_manager.clone(),
            self.tool_calls.clone(),
            self.messages.clone(),
            self.user_service.clone(),
        );
        match consolidator.run().await {
            // Every transition persisted on its way through, `Done` included, so there is
            // nothing left to write - only to report.
            Ok(()) => Ok(ctx.record().await.stats),
            Err(e) => {
                self.record_failure(&mut ctx.record().await, &e).await;
                Err(e)
            }
        }
    }

    /// The user's live pass, or a fresh one.
    ///
    /// At most one is open at a time - the sweep skips a user whose record is still
    /// backing off - so resuming the newest unfinished record is the whole of "resume".
    /// A record that reached `Done` is history: the next pass starts clean.
    pub(super) async fn open_record(
        &self,
        user_id: &str,
    ) -> Result<KnowledgeConsolidationRecord, AppError> {
        if let Some(open) = self
            .repo
            .latest_consolidation_record(user_id)
            .await?
            .filter(|r| !r.state.is_done())
        {
            if open.consolidation_id.is_empty() {
                tracing::warn!(
                    record = %open.id,
                    user = %open.user_id,
                    "pkm consolidation: resetting an incompatible development checkpoint"
                );
                self.repo.delete_consolidation_record(&open.id).await?;
            } else {
                return Ok(open);
            }
        }
        Ok(KnowledgeConsolidationRecord {
            id: crate::core::repository::new_id(),
            consolidation_id: crate::core::repository::new_id(),
            user_id: user_id.to_string(),
            state: ConsolidationStageState::Ingest(Default::default()),
            stats: ConsolidationStats::default(),
            attempts: 0,
            restart_count: 0,
            failure: None,
            next_attempt_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
    }

    /// A stage failed. Charge the attempt against the **current** stage and either back
    /// off, or give up on the pass entirely once the budget is gone.
    ///
    /// Abandoning is deliberate rather than retrying forever: an entity that fails
    /// deterministically would otherwise wedge every future pass for this user, and the
    /// work already committed is not lost - the entities it did not finish are still dirty,
    /// so the next pass picks them up from scratch.
    pub(super) async fn record_failure(
        &self,
        record: &mut KnowledgeConsolidationRecord,
        error: &AppError,
    ) {
        record.attempts += 1;
        let stage = record.state.label();
        let durable_author_retry = matches!(record.state, ConsolidationStageState::PlaybookAuthor);
        if !durable_author_retry
            && record.attempts >= self.memory_config.pkm_consolidation_max_attempts
        {
            if !matches!(record.state, ConsolidationStageState::Ingest(_)) {
                match self
                    .repo
                    .recover_or_fail_consolidation(
                        record,
                        &error.to_string(),
                        self.memory_config.pkm_consolidation_checkpoint_failure_cap,
                    )
                    .await
                {
                    Ok(recovered) => {
                        let terminal = matches!(recovered.state, ConsolidationStageState::Failed);
                        *record = recovered;
                        tracing::warn!(
                            error = %error,
                            user = %record.user_id,
                            stage,
                            restarts = record.restart_count,
                            terminal,
                            "pkm consolidation: exhausted stage retries; recovered from raw \
                             contributions or marked the checkpoint terminal"
                        );
                    }
                    Err(recovery_error) => tracing::warn!(
                        error = %recovery_error,
                        "pkm consolidation: fatal checkpoint recovery failed"
                    ),
                }
                return;
            }
            tracing::warn!(
                error = %error,
                user = %record.user_id,
                stage,
                attempts = record.attempts,
                "pkm consolidation: abandoning the pass — the stage never got past its \
                 retry budget. Its unfinished entities stay dirty and start fresh next pass."
            );
            if let Err(e) = self.repo.delete_consolidation_record(&record.id).await {
                tracing::warn!(error = %e, "pkm consolidation: dropping the record failed");
            }
            return;
        }
        let backoff = self
            .memory_config
            .pkm_consolidation_retry_base_secs
            .saturating_mul(1u64 << record.attempts.min(16));
        record.next_attempt_at = chrono::Utc::now() + chrono::Duration::seconds(backoff as i64);
        tracing::warn!(
            error = %error,
            user = %record.user_id,
            stage,
            attempts = record.attempts,
            backoff_secs = backoff,
            "pkm consolidation: stage failed — the pass will resume here"
        );
        if let Err(e) = self.repo.save_consolidation_record(record).await {
            tracing::warn!(error = %e, "pkm consolidation: parking the record failed");
        }
    }

    /// The background model group (`memory.model_group` → `primary`). Resolved lazily
    /// so a missing group degrades here rather than blocking startup.
    fn consolidation_model_group(&self) -> Result<crate::inference::config::ModelGroup, AppError> {
        resolve_model_group(&self.registry, &self.memory_config.model_group)
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "pkm consolidation: model group '{}' (and fallback 'primary') undefined",
                    self.memory_config.model_group
                ))
            })
            .cloned()
    }

    fn context(
        &self,
        scope: ConsolidationScope,
        harness: Arc<crate::agent::harness::Harness>,
        model_group: crate::inference::config::ModelGroup,
        record: KnowledgeConsolidationRecord,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> Arc<ConsolidationContext> {
        let llm = ConsolidationInference::with_cancel_token(
            harness,
            model_group,
            self.prompts.clone(),
            scope.user_id.clone(),
            cancel_token,
        );
        Arc::new(ConsolidationContext::new(
            scope,
            self.repo.clone(),
            self.storage.clone(),
            llm,
            record,
        ))
    }

    /// Reconcile the on-disk vault against the DB - repairs any file rename left
    /// half-applied by a crash (relocate mislocated files, re-render missing ones,
    /// drop stale duplicates). Idempotent and LLM-free; run once at boot. See
    /// `recovery::reconcile_user_files`.
    pub async fn reconcile_vault(&self) -> Result<(), AppError> {
        let mut by_user = std::collections::HashMap::new();
        for page in self.repo.all_internal_entities().await? {
            by_user
                .entry(page.user_id.clone())
                .or_insert_with(Vec::new)
                .push(page);
        }
        let mut report = recovery::ReconcileReport::default();
        for (user_id, pages) in by_user {
            let Some(_operation) = self.operations.try_begin_write(&user_id) else {
                continue;
            };
            let user_report = recovery::reconcile_user_files(
                &self.repo,
                &self.storage,
                &self.user_service,
                &user_id,
                &pages,
            )
            .await?;
            report.relocated += user_report.relocated;
            report.rerendered += user_report.rerendered;
            report.deduped += user_report.deduped;
        }
        if report.relocated + report.rerendered + report.deduped > 0 {
            tracing::info!(
                relocated = report.relocated,
                rerendered = report.rerendered,
                deduped = report.deduped,
                "pkm vault recovery repaired the projection"
            );
        }
        Ok(())
    }
}
