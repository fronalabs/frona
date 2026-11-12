//! Crash recovery for the vault projection.
//!
//! The `.md` files under `<handle>/pkm` are a *deterministic* projection of the DB
//! (`KnowledgeEntity.body` + metadata / links). A page rename
//! touches both the DB and the filesystem, so a crash mid-rename can desync them.
//! [`reconcile_user_files`] repairs that at boot - with no LLM call and no content loss -
//! by matching each file to its DB page via the stamped `uid` frontmatter:
//!
//!   - canonical file matches durable bytes      → leave it
//!   - canonical file differs from durable bytes → replace it from the database
//!   - file at the wrong path (matched by `uid`)  → move it to the canonical path
//!   - `uid` present at several paths             → keep canonical, delete stale copies
//!   - no file for a page                         → re-render deterministically from `body`
//!   - file whose `uid` maps to no page / has none → user-owned; left untouched
//!
//! The file's stamped `uid` is a durable identity independent of its (mutable) path, so
//! recovery reconciles by identity rather than replaying a write-ahead intent log - and
//! since every step is idempotent and content-preserving, no DB transaction is needed.

use std::collections::HashMap;

use tracing::warn;

use crate::auth::user_service::UserService;
use crate::core::error::AppError;
use crate::db::repo::pkm::PkmRepo;

use super::model::KnowledgeEntity;
use super::page::projection::write_page_and_rev;
use super::storage::PkmStorage;
use super::vault::VaultScope;
use frona_pkm::page::projection::{MarkdownPage, compose_page};

#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub relocated: usize,
    pub rerendered: usize,
    pub deduped: usize,
}

pub(super) async fn reconcile_user_files(
    repo: &PkmRepo,
    storage: &PkmStorage,
    user_service: &UserService,
    user_id: &str,
    pages: &[KnowledgeEntity],
) -> Result<ReconcileReport, AppError> {
    let mut report = ReconcileReport::default();
    let handle = match user_service.find_by_id(user_id).await {
        Ok(Some(user)) => user.handle,
        Ok(None) => return Ok(report),
        Err(error) => {
            warn!(%error, %user_id, "pkm recovery: user lookup failed");
            return Ok(report);
        }
    };
    let vault = match VaultScope::resolve(user_service, storage, user_id, &handle).await {
        Ok(vault) => vault,
        Err(error) => {
            warn!(%error, %user_id, "pkm recovery: vault scope failed");
            return Ok(report);
        }
    };

    // Common startup path: stream each canonical file through BLAKE3 and compare it
    // with the exact bytes held by the database. This avoids SHA-256 and avoids a
    // directory walk when every canonical file is present.
    let mut has_missing = false;
    for page in pages {
        match storage.page_fingerprint(&vault, &page.path)? {
            Some(file_fingerprint) => {
                if let Some(content) = page.sync_content.as_deref() {
                    if file_fingerprint != PkmStorage::content_fingerprint(content) {
                        storage.write_page(&vault, &page.path, content)?;
                        report.rerendered += 1;
                    }
                } else {
                    adopt_legacy_file(repo, storage, &vault, page).await?;
                }
            }
            None => has_missing = true,
        }
    }
    if !has_missing {
        return Ok(report);
    }

    let mut uid_locations: HashMap<String, Vec<String>> = HashMap::new();
    for (path, uid) in storage.list_page_files(&vault) {
        if let Some(uid) = uid {
            uid_locations.entry(uid).or_default().push(path);
        }
    }

    for page in pages {
        let locs = uid_locations.get(&page.id).cloned().unwrap_or_default();
        let at_canonical = locs.iter().any(|location| location == &page.path);
        let stale: Vec<String> = locs
            .into_iter()
            .filter(|location| *location != page.path)
            .collect();

        if at_canonical {
            report.deduped += delete_all(storage, &vault, &stale, &page.path);
            continue;
        }

        if let Some(content) = page.sync_content.as_deref() {
            storage.write_page(&vault, &page.path, content)?;
            report.rerendered += 1;
            report.deduped += delete_all(storage, &vault, &stale, &page.path);
        } else if let Some(source) = stale.first() {
            if let Err(error) = storage.move_page_file(&vault, source, &page.path) {
                warn!(%error, from = %source, to = %page.path, "pkm recovery: relocate failed");
            } else {
                report.relocated += 1;
                adopt_legacy_file(repo, storage, &vault, page).await?;
            }
            report.deduped += delete_all(storage, &vault, &stale[1..], &page.path);
        } else {
            match render_page_from_db(repo, storage, &vault, page).await {
                Ok(()) => report.rerendered += 1,
                Err(error) => {
                    warn!(%error, page = %page.path, "pkm recovery: re-render failed")
                }
            }
        }
    }
    Ok(report)
}

/// One-time compatibility path for pages written before exact canonical bytes were
/// durable. Preserve the existing file, then make those bytes authoritative.
async fn adopt_legacy_file(
    repo: &PkmRepo,
    storage: &PkmStorage,
    vault: &VaultScope,
    page: &KnowledgeEntity,
) -> Result<(), AppError> {
    let Some(content) = storage.read_page(vault, &page.path) else {
        return Ok(());
    };
    let rev = frona_pkm::page::projection::sha256_hex(&content);
    repo.set_page_projection(&page.user_id, &page.path, &content, &rev)
        .await
}

fn delete_all(
    storage: &PkmStorage,
    vault: &VaultScope,
    stale: &[String],
    page_path: &str,
) -> usize {
    let mut n = 0;
    for s in stale {
        if let Err(e) = storage.delete_page(vault, s) {
            warn!(error = %e, page = %page_path, dup = %s, "pkm recovery: dedup delete failed");
        } else {
            n += 1;
        }
    }
    n
}

/// Deterministically render a page's `.md` from DB state (stored `body` + attributes +
/// links) to its canonical path. No LLM - the exact inverse of what the author stage
/// persisted.
async fn render_page_from_db(
    repo: &PkmRepo,
    storage: &PkmStorage,
    vault: &VaultScope,
    page: &KnowledgeEntity,
) -> Result<(), AppError> {
    if let Some(content) = page.sync_content.as_deref().filter(|content| {
        page.rev
            .as_deref()
            .is_some_and(|rev| frona_pkm::page::projection::sha256_hex(content) == rev)
    }) {
        write_page_and_rev(repo, storage, vault, &page.user_id, &page.path, content).await?;
        return Ok(());
    }
    let links = repo
        .links_from_entity(&page.user_id, &page.path)
        .await
        .unwrap_or_default();
    let article = MarkdownPage::parse(&page.body);
    let file = compose_page(
        &page.clone().into(),
        &article,
        &page.attributes,
        &links.into_iter().map(Into::into).collect::<Vec<_>>(),
        &frona_pkm::ontology::PrefixMap::standard(),
        &frona_pkm::page::PageLayout::new(vault.directory())?,
    );
    write_page_and_rev(repo, storage, vault, &page.user_id, &page.path, &file).await?;
    Ok(())
}
