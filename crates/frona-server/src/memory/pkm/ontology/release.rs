//! The shipped ontology release: verifying the copy on disk, and fetching a
//! replacement when it is missing or damaged.
//!
//! The catalogue is baked into the image at build time, so in the normal case this
//! module does one thing - hash what is there against the manifest and conclude that
//! it is fine. Nothing is downloaded because a newer release exists upstream; a
//! release arrives with an image, not behind the operator's back. Reasoning that
//! changed on its own would be indistinguishable from reasoning that changed because
//! someone edited an entity.
//!
//! The fetch path exists for one case: the artifacts are absent or corrupt, which
//! otherwise leaves the PKM backend permanently unable to classify.

use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::core::error::AppError;

const MANIFEST: &str = "metadata.json";

/// Where a repaired copy is installed, relative to the user ontology directory.
///
/// Dotted, and a *subdirectory*: the catalogue scans a root non-recursively and only
/// picks up files with an ontology extension, so this is invisible to the scan of the
/// user root. It has to be somewhere the user root is not, because a repaired copy and
/// the image's copy declare the same `owl:Ontology` IRIs - loading both would trip the
/// duplicate-identity check and leave the server with no catalogue at all.
const REPAIR_SUBDIR: &str = ".release";

const REPO: &str = "fronalabs/frona-ontologies";

#[derive(Debug, Deserialize)]
struct Manifest {
    sources: Vec<SourceMeta>,
}

#[derive(Debug, Deserialize)]
struct SourceMeta {
    artifact: Artifact,
}

#[derive(Debug, Deserialize)]
struct Artifact {
    name: String,
    /// Over the **uncompressed** bytes. gzip embeds a timestamp, so hashing the
    /// archive would differ on every rebuild of identical content.
    content_sha256: String,
}

/// Why a release directory cannot be trusted. Carried rather than reduced to a bool so
/// the log says which file and what happened - "the ontology is broken" is not an
/// actionable message at 3am.
#[derive(Debug)]
pub enum Invalid {
    NoManifest,
    Unreadable(String),
    Missing(String),
    Corrupt {
        file: String,
        expected: String,
        found: String,
    },
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoManifest => write!(f, "no {MANIFEST}"),
            Self::Unreadable(e) => write!(f, "unreadable {MANIFEST}: {e}"),
            Self::Missing(name) => write!(f, "{name} is listed in {MANIFEST} but absent"),
            Self::Corrupt {
                file,
                expected,
                found,
            } => write!(
                f,
                "{file} does not match {MANIFEST}: expected {}, found {}",
                &expected[..expected.len().min(12)],
                &found[..found.len().min(12)]
            ),
        }
    }
}

/// Does `dir` hold a complete, intact release?
///
/// Every artifact the manifest lists must be present and hash to what it claims. A
/// partially-copied or truncated artifact parses perfectly well and simply contains
/// fewer terms - classification then degrades with nothing reporting a problem, which
/// is the failure mode this whole subsystem is built to avoid.
pub fn verify(dir: &Path) -> Result<(), Invalid> {
    let manifest = dir.join(MANIFEST);
    if !manifest.exists() {
        return Err(Invalid::NoManifest);
    }
    let raw = std::fs::read(&manifest).map_err(|e| Invalid::Unreadable(e.to_string()))?;
    let manifest: Manifest =
        serde_json::from_slice(&raw).map_err(|e| Invalid::Unreadable(e.to_string()))?;

    for source in &manifest.sources {
        let path = dir.join(&source.artifact.name);
        if !path.exists() {
            return Err(Invalid::Missing(source.artifact.name.clone()));
        }
        let found = sha256_uncompressed(&path)
            .map_err(|e| Invalid::Unreadable(format!("{}: {e}", source.artifact.name)))?;
        if found != source.artifact.content_sha256 {
            return Err(Invalid::Corrupt {
                file: source.artifact.name.clone(),
                expected: source.artifact.content_sha256.clone(),
                found,
            });
        }
    }
    Ok(())
}

/// Hash an artifact's *content*, decompressing on the fly. Streamed: KBpedia is 15 MB
/// uncompressed and there is no reason to hold it.
fn sha256_uncompressed(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut reader: Box<dyn std::io::Read> =
        if path.extension().and_then(|e| e.to_str()) == Some("gz") {
            Box::new(GzDecoder::new(std::io::BufReader::new(file)))
        } else {
            Box::new(std::io::BufReader::new(file))
        };
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}
pub fn repair_dir(user_dir: &Path) -> PathBuf {
    user_dir.join(REPAIR_SUBDIR)
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Fetch the latest published release into `dir`, then verify it.
///
/// Downloads to a scratch directory and moves into place only once the manifest
/// checks out, so a half-finished fetch never becomes the thing the next boot loads
/// and trusts.
pub async fn fetch_latest(dir: &Path) -> Result<String, AppError> {
    let client = reqwest::Client::builder()
        .user_agent("frona-server")
        .build()
        .map_err(|e| AppError::Internal(format!("ontology: http client: {e}")))?;

    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let release: Release = client
        .get(&url)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("ontology: fetch release list: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::Internal(format!("ontology: fetch release list: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("ontology: parse release list: {e}")))?;

    let wanted: Vec<&Asset> = release
        .assets
        .iter()
        .filter(|a| a.name.ends_with(".ttl.gz") || a.name == MANIFEST || a.name == "NOTICE")
        .collect();
    if !wanted.iter().any(|a| a.name.ends_with(".ttl.gz")) {
        return Err(AppError::Internal(format!(
            "ontology: release {} publishes no artifacts",
            release.tag_name
        )));
    }

    let staging = dir.with_extension("partial");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| AppError::Internal(format!("ontology: create {}: {e}", staging.display())))?;

    for asset in &wanted {
        let bytes = client
            .get(&asset.browser_download_url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| AppError::Internal(format!("ontology: fetch {}: {e}", asset.name)))?
            .bytes()
            .await
            .map_err(|e| AppError::Internal(format!("ontology: read {}: {e}", asset.name)))?;
        std::fs::write(staging.join(&asset.name), &bytes)
            .map_err(|e| AppError::Internal(format!("ontology: write {}: {e}", asset.name)))?;
    }

    // Verify *before* publishing. A release that fails its own manifest is a bad
    // release, and installing it would only move the problem to the next boot.
    verify(&staging).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        AppError::Internal(format!(
            "ontology: downloaded release {} is invalid: {e}",
            release.tag_name
        ))
    })?;

    let _ = std::fs::remove_dir_all(dir);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AppError::Internal(format!("ontology: create {}: {e}", parent.display()))
        })?;
    }
    std::fs::rename(&staging, dir)
        .map_err(|e| AppError::Internal(format!("ontology: install {}: {e}", dir.display())))?;

    Ok(release.tag_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn artifact(dir: &Path, name: &str, content: &str) -> String {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(content.as_bytes()).unwrap();
        std::fs::write(dir.join(name), enc.finish().unwrap()).unwrap();
        hex::encode(Sha256::digest(content.as_bytes()))
    }

    fn manifest(dir: &Path, entries: &[(&str, &str)]) {
        let sources: Vec<String> = entries
            .iter()
            .map(|(name, sha)| {
                format!(r#"{{"artifact":{{"name":"{name}","content_sha256":"{sha}"}}}}"#)
            })
            .collect();
        std::fs::write(
            dir.join(MANIFEST),
            format!(r#"{{"sources":[{}]}}"#, sources.join(",")),
        )
        .unwrap();
    }

    #[test]
    fn intact_release_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = artifact(tmp.path(), "a.ttl.gz", "<x> <y> <z> .\n");
        manifest(tmp.path(), &[("a.ttl.gz", &sha)]);
        assert!(verify(tmp.path()).is_ok());
    }

    /// The hash is over the *uncompressed* bytes, because gzip embeds a timestamp:
    /// hashing the archive would report a change on every rebuild of identical content.
    #[test]
    fn hash_ignores_gzip_framing() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "<x> <y> <z> .\n";
        let first = artifact(tmp.path(), "a.ttl.gz", content);
        // Re-compress the same content at a different level - different bytes on disk.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(content.as_bytes()).unwrap();
        let recompressed = enc.finish().unwrap();
        let original = std::fs::read(tmp.path().join("a.ttl.gz")).unwrap();
        assert_ne!(original, recompressed, "the archives really do differ");
        std::fs::write(tmp.path().join("a.ttl.gz"), &recompressed).unwrap();

        manifest(tmp.path(), &[("a.ttl.gz", &first)]);
        assert!(
            verify(tmp.path()).is_ok(),
            "same content, so it still verifies"
        );
    }

    /// The case the check exists for: a truncated artifact parses fine and simply holds
    /// fewer terms, so nothing downstream would ever report a problem.
    #[test]
    fn truncated_artifact_is_corrupt_not_merely_smaller() {
        let tmp = tempfile::tempdir().unwrap();
        let sha = artifact(tmp.path(), "a.ttl.gz", "<x> <y> <z> .\n<p> <q> <r> .\n");
        artifact(tmp.path(), "a.ttl.gz", "<x> <y> <z> .\n");
        manifest(tmp.path(), &[("a.ttl.gz", &sha)]);
        let Err(Invalid::Corrupt { file, .. }) = verify(tmp.path()) else {
            panic!("a short artifact must not verify");
        };
        assert_eq!(file, "a.ttl.gz");
    }

    #[test]
    fn missing_artifact_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        manifest(tmp.path(), &[("gone.ttl.gz", "0")]);
        let Err(Invalid::Missing(name)) = verify(tmp.path()) else {
            panic!("a listed-but-absent artifact must not verify");
        };
        assert_eq!(name, "gone.ttl.gz");
    }

    #[test]
    fn empty_directory_has_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(verify(tmp.path()), Err(Invalid::NoManifest)));
    }

    /// The repair copy must not sit where the user root's scan would find it: the
    /// image's copy and a repaired copy declare the same ontology IRIs, and loading
    /// both trips the duplicate-identity check.
    #[test]
    fn repair_directory_hides_from_the_user_root_scan() {
        let user = Path::new("/data/ontology");
        let repair = repair_dir(user);
        assert_eq!(repair, Path::new("/data/ontology/.release"));
        assert_eq!(repair.parent(), Some(user), "inside the user root...");
        assert!(
            frona_pkm::ontology::catalogue::format_of(&repair).is_none(),
            "...but not itself a source the scan would pick up"
        );
    }
}
