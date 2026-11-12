//! Stateless Obsidian sync - the engine behind the `/api/memory/pkm/sync/*`
//! endpoints. The server holds **no** per-device or change-tracking state: it
//! exposes current page state (`manifest` + `pages`) and accepts CAS'd edits.
//! Each client diffs the manifest against its own `path → rev` map and
//! reconciles independently.

use std::collections::HashSet;
use std::sync::Arc;

use frona_harness::inference::InferenceError;
use tokio_util::sync::CancellationToken;

use crate::agent::harness::Harness;
use crate::auth::user_service::UserService;
use crate::core::Handle;
use crate::core::config::MemoryConfig;
use crate::core::error::AppError;
use crate::core::user_config::{UserConfigPatch, UserMemoryConfig};
use crate::db::repo::pkm::{
    PageEditBase, PageEditCommit, PageEditMemoryOp, PageEditWrite, PkmRepo,
};
use crate::inference::provider::service::ModelProviderService;
use frona_harness::inference::ModelGroup;
use frona_text::prompt::PromptLoader;

use super::consolidation::{ConsolidationContext, ConsolidationInference, ConsolidationScope};
use super::ingest::Ingest;
use super::page::projection;
use super::rename;
use super::storage::PkmStorage;
use super::vault::VaultScope;
use frona_pkm::page::projection::sha256_hex;

/// The CAS decision for an incoming human edit - compares the client's `base_rev`
/// to the page's current head. Pure (no mutation): the write-back (memory ops +
/// body adoption) is applied only on `Apply`/`New`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditGate {
    /// `base_rev` matches head → safe to apply to this existing page. Carries the
    /// current head bytes so the write-back can diff incoming against them.
    Apply {
        clean_path: String,
        head_content: String,
    },
    /// No such page → a human-authored **new** page (extract memories from body).
    New { clean_path: String },
    /// Stale `base_rev` → the client must pull head and reapply.
    Conflict {
        head_rev: String,
        head_content: String,
    },
}

/// One push item on `/sync/edits`.
pub enum EditOp {
    Upsert {
        path: String,
        base_rev: Option<String>,
        content: String,
    },
    Delete {
        path: String,
    },
}

impl EditOp {
    fn path(&self) -> &str {
        match self {
            EditOp::Upsert { path, .. } | EditOp::Delete { path } => path,
        }
    }
}

/// The per-item outcome of an `/sync/edits` push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditResult {
    Accepted {
        rev: String,
    },
    Created {
        rev: String,
    },
    Conflict {
        head_rev: String,
        head_content: String,
    },
    Removed,
    Indexed,
    Unchanged,
    /// A directory rename moved `count` pages under the source prefix. Per-page revs
    /// aren't returned - the client re-diffs the manifest to pick up the new paths.
    Renamed {
        count: usize,
    },
}

/// One manifest entry - a Memory page's vault path (`<directory>/<path>`) + its `rev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: String,
    pub rev: String,
}

/// A page's content for the `pages` fetch step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageContent {
    pub path: String,
    pub rev: String,
    pub content: String,
}

/// The Obsidian sync engine - the read and write sides of stateless sync.
///
/// A **peer** of [`PkmService`](super::PkmService), assembled over the same `PkmRepo` and
/// `PkmStorage`. `AppState` owns both services.
#[derive(Clone)]
pub struct PkmSyncService {
    repo: Arc<PkmRepo>,
    storage: PkmStorage,
    /// For building an extraction `Ingest` on External ingest (short-memory
    /// decay params) and the self-page write-through.
    memory_config: MemoryConfig,
    user_service: UserService,
    prompts: PromptLoader,
    /// Resolves the background model group lazily by name (see [`model_group`]).
    model_providers: Arc<ModelProviderService>,
    operations: super::operations::PkmOperationCoordinator,
    cancellation: CancellationToken,
}

impl PkmSyncService {
    pub fn new(
        repo: Arc<PkmRepo>,
        storage: PkmStorage,
        memory_config: MemoryConfig,
        user_service: UserService,
        prompts: PromptLoader,
        model_providers: Arc<ModelProviderService>,
    ) -> Self {
        Self::with_operations(
            repo,
            storage,
            memory_config,
            user_service,
            prompts,
            model_providers,
            super::operations::PkmOperationCoordinator::default(),
        )
    }

    pub(crate) fn with_operations(
        repo: Arc<PkmRepo>,
        storage: PkmStorage,
        memory_config: MemoryConfig,
        user_service: UserService,
        prompts: PromptLoader,
        model_providers: Arc<ModelProviderService>,
        operations: super::operations::PkmOperationCoordinator,
    ) -> Self {
        Self {
            repo,
            storage,
            memory_config,
            user_service,
            prompts,
            model_providers,
            operations,
            cancellation: CancellationToken::new(),
        }
    }

    /// Bind cancellation to this sync request's planning and extraction work.
    /// Already committed edits or mirrored external notes are not rolled back;
    /// unfinished extraction remains pending for an explicit subsequent push.
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    fn check_cancelled(&self) -> Result<(), AppError> {
        if self.cancellation.is_cancelled() {
            return Err(AppError::Inference(Box::new(InferenceError::Cancelled(
                "PKM sync cancelled".into(),
            ))));
        }
        Ok(())
    }

    /// Resolve the background model group by name (`memory.model_group`, falling
    /// back to `primary`). Lookup is pure and uses the startup-compiled groups.
    fn model_group(&self) -> Result<ModelGroup, AppError> {
        super::resolve_model_group(&self.model_providers, &self.memory_config.model_group)
            .ok_or_else(|| AppError::Internal("pkm sync: memory model group undefined".into()))
    }

    /// Where this user's files live. Resolved once per request; the single lookup that
    /// every path operation below shares.
    async fn vault(&self, user_id: &str, handle: &Handle) -> Result<VaultScope, AppError> {
        VaultScope::resolve(&self.user_service, &self.storage, user_id, handle).await
    }

    /// The user's Memory directory name (default `Memory`) - the `/sync/config` value.
    pub async fn memory_directory(&self, user_id: &str) -> Result<String, AppError> {
        Ok(self
            .user_service
            .user_config(user_id)
            .await?
            .memory
            .shared_vault_directory)
    }

    /// Set the user's Memory directory name - a single clean segment (no `/`, no
    /// `..`). Validated here (the memory domain owns the rule) then persisted through
    /// the user-config CAS. Takes effect on the next render/manifest - DB page paths
    /// stay clean, so renaming is a re-projection.
    pub async fn set_memory_directory(&self, user_id: &str, name: &str) -> Result<(), AppError> {
        let _operation = self
            .operations
            .try_begin_write(user_id)
            .ok_or_else(|| AppError::Conflict("PKM reset is in progress".into()))?;
        let clean = PkmStorage::validate_directory(name)?.to_string();
        let current = self.user_service.user_config(user_id).await?;
        let patch = UserConfigPatch {
            memory: Some(UserMemoryConfig {
                shared_vault_directory: clean,
            }),
        };
        self.user_service
            .update_user_config(user_id, current.updated_at, patch)
            .await?;
        Ok(())
    }

    /// The full `(vault_path, rev)` manifest of rendered Memory pages. Cheap (a few
    /// KB); the client diffs it against its local rev map to decide what to fetch
    /// and which local files to delete (absent = deleted).
    pub async fn manifest(&self, user_id: &str) -> Result<Vec<ManifestEntry>, AppError> {
        let directory = self.memory_directory(user_id).await?;
        let rows = self.repo.page_manifest(user_id).await?;
        Ok(rows
            .into_iter()
            .map(|(path, rev)| ManifestEntry {
                path: format!("{directory}/{path}"),
                rev,
            })
            .collect())
    }

    /// Content for the requested vault paths (the manifest-diff's fetch step, and
    /// bootstrap when the client passes every path). A path that isn't a current
    /// rendered Memory page (missing, un-rendered, or outside the Memory directory) is
    /// simply omitted - the client already treats manifest-absence as a delete.
    pub async fn get_pages(
        &self,
        user_id: &str,
        handle: &Handle,
        vault_paths: &[String],
    ) -> Result<Vec<PageContent>, AppError> {
        let vault = self.vault(user_id, handle).await?;
        let mut out = Vec::new();
        for vp in vault_paths {
            let Some(clean) = vault.page_from_any(vp) else {
                continue;
            };
            let Some(page) = self.repo.entity_by_path(user_id, &clean).await? else {
                continue;
            };
            let Some(rev) = page.rev.clone() else {
                continue;
            };
            let Some(content) = page
                .sync_content
                .filter(|content| sha256_hex(content) == rev)
                .or_else(|| self.storage.read_page(&vault, &clean))
            else {
                continue;
            };
            out.push(PageContent {
                path: vault.vault_path(&clean),
                rev,
                content,
            });
        }
        Ok(out)
    }

    /// The CAS gate for an incoming edit: resolve the vault path to a clean
    /// page path, then compare `base_rev` to the page's current `rev`. Read-only -
    /// the caller applies the write-back only on `Apply`/`New`.
    pub async fn check_edit(
        &self,
        user_id: &str,
        handle: &Handle,
        vault_path: &str,
        base_rev: Option<&str>,
    ) -> Result<EditGate, AppError> {
        let vault = self.vault(user_id, handle).await?;
        let clean = vault.page_from_any(vault_path).ok_or_else(|| {
            AppError::Validation(format!("path not under the Memory directory: {vault_path}"))
        })?;
        match self.repo.entity_by_path(user_id, &clean).await? {
            None => Ok(EditGate::New { clean_path: clean }),
            Some(page) => {
                let head_rev = page.rev.unwrap_or_default();
                let head_content = page
                    .sync_content
                    .filter(|content| sha256_hex(content) == head_rev)
                    .or_else(|| self.storage.read_page(&vault, &clean))
                    .unwrap_or_default();
                match base_rev {
                    Some(b) if b == head_rev && !head_rev.is_empty() => Ok(EditGate::Apply {
                        clean_path: clean,
                        head_content,
                    }),
                    _ => Ok(EditGate::Conflict {
                        head_rev,
                        head_content,
                    }),
                }
            }
        }
    }

    /// Apply one **Internal** push item (the caller routes External paths to Phase
    /// F). Upsert → CAS then human write-back (existing) or create+extract (new),
    /// adopting the human's file verbatim (no re-author). Delete → retire the
    /// page's memories as erroneous and drop the projection.
    pub async fn apply_edit(
        &self,
        harness: &Arc<Harness>,
        user_id: &str,
        handle: &Handle,
        op: EditOp,
    ) -> Result<EditResult, AppError> {
        self.check_cancelled()?;
        let _operation = self
            .operations
            .try_begin_write(user_id)
            .ok_or_else(|| AppError::Conflict("PKM reset is in progress".into()))?;
        let vault = self.vault(user_id, handle).await?;
        self.check_cancelled()?;
        // Route by prefix: a path under the Memory directory is Internal (bidirectional
        // write-back); anything else is External (User Vault) → read-only ingest.
        let Some(clean) = vault.page_from_any(op.path()) else {
            return self.ingest_external(harness, handle, user_id, op).await;
        };

        match op {
            EditOp::Delete { .. } => {
                // Retire the facts (global, non-destructive) and drop the projection
                // node + file so the page vanishes from the manifest immediately.
                self.repo
                    .mark_entity_memories_erroneous(user_id, &clean)
                    .await?;
                self.repo.delete_entity(user_id, &clean).await?;
                let _ = self.storage.delete_page(&vault, &clean);
                Ok(EditResult::Removed)
            }
            // This arm applies the CAS verdict from `check_edit`. `path` is the original
            // vault path, not `clean`:
            // `check_edit` does its own resolution.
            EditOp::Upsert {
                path,
                base_rev,
                content,
            } => {
                let gate = self
                    .check_edit(user_id, handle, &path, base_rev.as_deref())
                    .await?;
                let (clean_path, head_content, base, new_page_name, created) = match gate {
                    EditGate::New { clean_path } => {
                        let name = clean_path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&clean_path)
                            .to_string();
                        (
                            clean_path,
                            String::new(),
                            PageEditBase::Missing,
                            Some(name),
                            true,
                        )
                    }
                    EditGate::Apply {
                        clean_path,
                        head_content,
                    } => {
                        let expected =
                            base_rev.expect("an applied edit has a matching base revision");
                        (
                            clean_path,
                            head_content,
                            PageEditBase::Revision(expected),
                            None,
                            false,
                        )
                    }
                    EditGate::Conflict {
                        head_rev,
                        head_content,
                    } => {
                        return Ok(EditResult::Conflict {
                            head_rev,
                            head_content,
                        });
                    }
                };

                // Inference produces a plan only. No durable mutation happens before
                // the repository confirms the base revision in its transaction.
                let memory_ops = self
                    .plan_writeback(harness, user_id, &clean_path, &head_content, &content)
                    .await?;
                let rev = sha256_hex(&content);
                let write = PageEditWrite {
                    base,
                    new_page_name,
                    body: frona_pkm::page::projection::extract_body(&content),
                    content: content.clone(),
                    rev: rev.clone(),
                    memory_ops,
                };

                // Keep the file finalization ordered with same-process edits. The
                // repository CAS is still required for writers in other processes.
                let _page_edit = self.operations.begin_page_edit(user_id, &clean_path).await;
                self.check_cancelled()?;
                match self
                    .repo
                    .commit_page_edit_cas(user_id, &clean_path, &write)
                    .await?
                {
                    PageEditCommit::Applied => {
                        self.storage.write_page(&vault, &clean_path, &content)?;
                        if created {
                            Ok(EditResult::Created { rev })
                        } else {
                            Ok(EditResult::Accepted { rev })
                        }
                    }
                    PageEditCommit::Conflict {
                        head_rev,
                        head_content,
                    } => Ok(EditResult::Conflict {
                        head_rev: head_rev.unwrap_or_default(),
                        head_content: head_content
                            .or_else(|| self.storage.read_page(&vault, &clean_path))
                            .unwrap_or_default(),
                    }),
                }
            }
        }
    }

    /// External (User Vault) ingest: mirror the note read-only + upsert an
    /// `origin=External` page so it's searchable and the agent can `read`/`grep`/
    /// cite it. It also extracts `source=external` memories synchronously. The
    /// system never writes back to the user's note.
    async fn ingest_external(
        &self,
        harness: &Arc<Harness>,
        handle: &Handle,
        user_id: &str,
        op: EditOp,
    ) -> Result<EditResult, AppError> {
        match op {
            EditOp::Upsert { path, content, .. } => {
                let path = path.trim_end_matches(".md").to_string();
                let rev = sha256_hex(&content);
                // Serialize same-process pushes for this note. The durable revision
                // checks below also reject stale work from another process.
                let _page_edit = self.operations.begin_page_edit(user_id, &path).await;
                self.check_cancelled()?;
                let progress = self
                    .repo
                    .upsert_external_page(user_id, &path, &content, &rev)
                    .await?;
                if progress.is_complete() {
                    return Ok(EditResult::Unchanged);
                }
                if progress.mirror_pending {
                    self.storage.write_user_note(handle, &path, &content)?;
                    if !self
                        .repo
                        .mark_external_page_mirrored(user_id, &path, &progress.rev)
                        .await?
                    {
                        return Ok(EditResult::Indexed);
                    }
                }
                // Replacement is atomic: old derived memories remain available until
                // the new batch and extracted revision commit together. Extraction is
                // non-fatal because the accepted note remains mirrored and searchable.
                if progress.extraction_pending
                    && let Err(e) = self
                        .extract_external(harness, user_id, handle, &path, &content, &progress.rev)
                        .await
                {
                    tracing::warn!(error = %e, note = %path, "pkm sync: external extraction failed");
                }
                Ok(EditResult::Indexed)
            }
            EditOp::Delete { path } => {
                let path = path.trim_end_matches(".md").to_string();
                self.repo.delete_external_page(user_id, &path).await?;
                let _ = self.storage.delete_user_note(handle, &path);
                Ok(EditResult::Removed)
            }
        }
    }

    /// Mine a User Vault note into the entity graph via the extraction engine -
    /// same entity resolution as chat consolidation, but memories are stamped
    /// `source=External { note }` (the note's vault path). The affected entity
    /// pages reconcile and author on the next consolidation pass.
    async fn extract_external(
        &self,
        harness: &Arc<Harness>,
        user_id: &str,
        handle: &Handle,
        note_path: &str,
        content: &str,
        expected_rev: &str,
    ) -> Result<(), AppError> {
        // Detached pass: no chat, and it runs as the user's `system` agent (the base
        // policy grants it the read-only investigator tools).
        let system_agent = harness.agent_service.system_agent(user_id).await?;
        let scope = ConsolidationScope {
            user_id: user_id.to_string(),
            user_name: String::new(),
            agent_id: system_agent.id,
            chat_id: None,
            vault: self.vault(user_id, handle).await?,
            temporal_sources: Vec::new(),
            evidence_sources: vec![frona_pkm::TranscriptEvidenceSource {
                handle: "m1".to_string(),
                text: content.to_string(),
                kind: frona_pkm::TranscriptEvidenceKind::ExternalNote {
                    note: note_path.to_string(),
                },
            }],
            recall: Default::default(),
            timezone: "UTC".to_string(),
        };
        // Mine with the same extraction operation as the sweep. The pages it mints land
        // in the user's dirty set and are typed by the next sweep's consolidation.
        let llm = ConsolidationInference::with_cancel_token(
            harness.clone(),
            self.model_group()?,
            self.prompts.clone(),
            user_id.to_string(),
            self.cancellation.clone(),
        );
        let ctx = Arc::new(ConsolidationContext::detached(
            scope,
            self.repo.clone(),
            self.storage.clone(),
            llm,
        ));
        let transcript = super::ingest::transcript::external_note(content);
        let batch = Ingest::new(ctx.clone()).run(&transcript).await?;
        let mut checkpoint = ctx.record().await;
        checkpoint.stats.absorb_ingest_batch(&batch);
        self.check_cancelled()?;
        self.repo
            .commit_external_extract_patch_with_checkpoint(
                user_id,
                note_path,
                expected_rev,
                &batch,
                &checkpoint,
            )
            .await?;
        Ok(())
    }

    /// Plan the human write-back from `head` → `incoming`. This function has no
    /// durable side effects. The repository later applies the validated operations
    /// with the page revision compare-and-set.
    async fn plan_writeback(
        &self,
        harness: &Arc<Harness>,
        user_id: &str,
        clean_path: &str,
        head: &str,
        incoming: &str,
    ) -> Result<Vec<PageEditMemoryOp>, AppError> {
        if frona_pkm::writeback::writeback_diff(head, incoming)
            .trim()
            .is_empty()
        {
            return Ok(Vec::new());
        }
        let memories = self.repo.memories_for_entity(user_id, clean_path).await?;
        let inference = ConsolidationInference::with_cancel_token(
            harness.clone(),
            self.model_group()?,
            self.prompts.clone(),
            user_id.into(),
            self.cancellation.clone(),
        );
        let defaults = frona_pkm::writeback::WritebackPrompts::default();
        let phase = frona_pkm::writeback::Writeback::new(inference.harness()).prompts(
            frona_pkm::writeback::WritebackPrompts {
                defaults: inference.prompt_defaults(),
                system: inference.prompt_source("pkm/writeback/system.md", &defaults.system)?,
                input: inference.prompt_source("pkm/writeback/input.md", &defaults.input)?,
            },
        );
        let operations = phase
            .run_with(
                frona_pkm::writeback::WritebackInput {
                    path: clean_path.into(),
                    head: head.into(),
                    incoming: incoming.into(),
                    memories: memories.into_iter().map(Into::into).collect(),
                },
                |run| inference.configure(run),
            )
            .await?;
        Ok(operations
            .into_iter()
            .map(|op| match op {
                frona_pkm::writeback::PageEditMemoryOp::Add { kind, content } => {
                    PageEditMemoryOp::Add {
                        kind: kind.into(),
                        content,
                    }
                }
                frona_pkm::writeback::PageEditMemoryOp::Supersede {
                    older_id,
                    kind,
                    content,
                    note,
                } => PageEditMemoryOp::Supersede {
                    older_id,
                    kind: kind.into(),
                    content,
                    note,
                },
                frona_pkm::writeback::PageEditMemoryOp::SetDisposition {
                    memory_id,
                    disposition,
                } => PageEditMemoryOp::SetDisposition {
                    memory_id,
                    disposition: disposition.into(),
                },
            })
            .collect())
    }

    /// Rename/move a Memory page, preserving identity: rewrite the DB edges, move
    /// the file, and fix inbound wikilinks (the sync-side twin of consolidation's
    /// `rename_page_everywhere`). `Conflict` if the target path is occupied.
    /// Rename a Memory page **or** an entire subdirectory. A `from_path` ending in
    /// `.md` renames a single page; otherwise it's a directory move - every page
    /// under `<from>/` is remapped to `<to>/`.
    ///
    /// Both sides rewrite links for their own copy: Obsidian updates the vault's
    /// `[[wikilinks]]` client-side, and the server rewrites its own page mirror
    /// ([`rename::rewrite_links_after_move`]) *and* repoints the path-keyed link tables
    /// ([`PkmRepo::rename_entity`]) so its state stays consistent immediately, not just
    /// after the next author pass re-derives the projection.
    pub async fn rename(
        &self,
        user_id: &str,
        handle: &Handle,
        from_path: &str,
        to_path: &str,
    ) -> Result<EditResult, AppError> {
        let _operation = self
            .operations
            .try_begin_write(user_id)
            .ok_or_else(|| AppError::Conflict("PKM reset is in progress".into()))?;
        let vault = self.vault(user_id, handle).await?;
        if from_path.trim_end_matches('/').ends_with(".md") {
            self.rename_one(user_id, &vault, from_path, to_path).await
        } else {
            self.rename_dir(user_id, &vault, from_path, to_path).await
        }
    }

    /// Resolve a vault path to a clean Memory page path (strips the directory prefix);
    /// errors if it isn't under the Memory directory.
    fn vault_to_page(
        &self,
        vault: &VaultScope,
        vault_path: &str,
        role: &str,
    ) -> Result<String, AppError> {
        vault.page_from_any(vault_path).ok_or_else(|| {
            AppError::Validation(format!("rename {role} not in Memory: {vault_path}"))
        })
    }

    /// Single-page rename - conflicts (non-destructively) if the target page exists.
    async fn rename_one(
        &self,
        user_id: &str,
        vault: &VaultScope,
        from_path: &str,
        to_path: &str,
    ) -> Result<EditResult, AppError> {
        let from = self.vault_to_page(vault, from_path, "source")?;
        let to = self.vault_to_page(vault, to_path, "target")?;
        if let Some(occupant) = self.repo.entity_by_path(user_id, &to).await? {
            return Ok(EditResult::Conflict {
                head_rev: occupant.rev.unwrap_or_default(),
                head_content: self.storage.read_page(vault, &to).unwrap_or_default(),
            });
        }
        let rev = self.move_page(user_id, vault, &from, &to).await?;
        Ok(EditResult::Accepted { rev })
    }

    /// Directory rename - remap every page under `<from>/` to `<to>/`. Rejects up
    /// front (before any write) if a target path is occupied by a page that isn't
    /// itself part of the move.
    async fn rename_dir(
        &self,
        user_id: &str,
        vault: &VaultScope,
        from_path: &str,
        to_path: &str,
    ) -> Result<EditResult, AppError> {
        let from = self.vault_to_page(vault, from_path, "source")?;
        let to = self.vault_to_page(vault, to_path, "target")?;
        let prefix = format!("{from}/");
        let moves: Vec<(String, String)> = self
            .repo
            .list_all_entity_paths(user_id)
            .await?
            .into_iter()
            .filter(|p| p.starts_with(&prefix))
            .map(|old| {
                let new = format!("{to}/{}", &old[prefix.len()..]);
                (old, new)
            })
            .collect();
        if moves.is_empty() {
            return Err(AppError::NotFound(format!(
                "no pages under directory: {from_path}"
            )));
        }
        let moving: HashSet<&str> = moves.iter().map(|(old, _)| old.as_str()).collect();
        for (_, new) in &moves {
            if !moving.contains(new.as_str())
                && self.repo.entity_by_path(user_id, new).await?.is_some()
            {
                return Err(AppError::Conflict(format!(
                    "rename target already exists: {new}"
                )));
            }
        }
        // Phase 1 - atomic: rename every page's record + edges in one transaction, so
        // a crash can't leave the directory half-moved (a split the boot reconcile
        // would cement rather than finish).
        self.repo.rename_entities(user_id, &moves).await?;
        // Phase 2 - projection (best-effort): move the files + re-stamp revs. The DB is
        // now the consistent source of truth, so a crash here just leaves files for the
        // boot `reconcile_files` pass to relocate.
        for (old, new) in &moves {
            let _ = self.storage.move_page_file(vault, old, new);
            // `None` = not projected; boot reconcile_files relocates + re-stamps.
            projection::restamp_rev(&self.repo, &self.storage, vault, user_id, new).await?;
        }
        // Then, once every page is at its final path, rewrite the mirror's
        // `[[wikilinks]]` to reflect each move (a linker may itself have been moved).
        for (old, new) in &moves {
            rename::rewrite_links_after_move(&self.repo, &self.storage, vault, user_id, old, new)
                .await;
        }
        Ok(EditResult::Renamed { count: moves.len() })
    }

    /// Move one page `from → to`: repoint its link tables + record, move the `.md`
    /// file, re-stamp its rev so the manifest reflects the new path. Returns the rev.
    /// Move one page `from → to` - the shared rename (DB edges, file, inbound wikilinks)
    /// plus the rev re-stamp the manifest needs. Returns the new rev.
    async fn move_page(
        &self,
        user_id: &str,
        vault: &VaultScope,
        from: &str,
        to: &str,
    ) -> Result<String, AppError> {
        rename::page_everywhere(&self.repo, &self.storage, vault, user_id, from, to).await?;
        // The rename moved the file, so the only way this is `None` is a projection that
        // was already missing - the client re-fetches on the mismatch, and boot recovery
        // re-renders it.
        Ok(
            projection::restamp_rev(&self.repo, &self.storage, vault, user_id, to)
                .await?
                .unwrap_or_default(),
        )
    }
}
