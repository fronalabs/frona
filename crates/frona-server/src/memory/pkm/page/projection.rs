//! Persist canonical page bytes and recover filesystem mirrors.
use crate::core::error::AppError;
use crate::db::repo::pkm::PkmRepo;
use crate::memory::pkm::{storage::PkmStorage, vault::VaultScope};
use frona_pkm::page::projection::sha256_hex;

pub(in crate::memory::pkm) fn parse_uid(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        line.strip_prefix("uid:")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Commit the exact canonical bytes and revision, then update the page's `.md` mirror.
/// A failed or interrupted mirror write remains recoverable from the database.
pub(crate) async fn write_page_and_rev(
    repo: &PkmRepo,
    storage: &PkmStorage,
    vault: &VaultScope,
    user_id: &str,
    path: &str,
    file: &str,
) -> Result<String, AppError> {
    let rev = sha256_hex(file);
    repo.set_page_projection(user_id, path, file, &rev).await?;
    storage.write_page(vault, path, file)?;
    Ok(rev)
}

/// Re-stamp a page's `rev` from whatever is on disk - the other half, for a page whose
/// bytes moved rather than changed (a rename). Returns the new rev, or `None` when there
/// is no file to hash.
///
/// Nothing is stamped in that case, deliberately. Hashing an absent file yields the
/// sha256 of the empty string, which is a *claim* - "this page is empty" - and a client
/// that stores it agrees with the server that a page with no file is in sync. Leaving the
/// previous rev in place keeps the row visibly out of step until the boot
/// `reconcile_files` pass re-renders the file and stamps it for real.
pub(crate) async fn restamp_rev(
    repo: &PkmRepo,
    storage: &PkmStorage,
    vault: &VaultScope,
    user_id: &str,
    path: &str,
) -> Result<Option<String>, AppError> {
    let Some(content) = storage.read_page(vault, path) else {
        return Ok(None);
    };
    let rev = sha256_hex(&content);
    repo.set_page_projection(user_id, path, &content, &rev)
        .await?;
    Ok(Some(rev))
}
