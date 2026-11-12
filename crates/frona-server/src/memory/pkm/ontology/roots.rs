use std::path::{Path, PathBuf};
use std::sync::Arc;

use frona_pkm::ontology::catalogue::roots::Root;

use crate::core::error::AppError;
use crate::memory::pkm::ontology::release;
use frona_pkm::ontology::OntologyCatalogue;
use frona_pkm::ontology::catalogue::ontology_files;

/// The two directories a catalogue is assembled from, and the only place that pairing
/// is spelled out. Carried rather than re-derived so the download task and the loader
/// cannot disagree about where the artifacts go.
#[derive(Clone, Debug)]
pub struct Roots {
    /// The release that ships in the image. **Read-only** - in a container this is a
    /// layer, so anything written here is gone on restart.
    pub release: PathBuf,
    /// The user's own ontologies, on the data volume. The only writable half, so this
    /// is also where anything fetched at runtime lands. Need not exist.
    pub user: PathBuf,
}

impl Roots {
    /// Scan both and absorb into one catalogue.
    ///
    /// `Release` goes first because attribution is assigned on first sight: a term the
    /// release declares has to stay attributed to the release.
    pub fn load(&self) -> Result<Arc<OntologyCatalogue>, AppError> {
        Ok(OntologyCatalogue::load(&[
            (Root::Release, &self.release_in_use()),
            (Root::User, &self.user),
        ])?)
    }

    /// Which directory actually supplies the release: the image's copy if it is
    /// usable, otherwise a repaired one.
    ///
    /// They are alternatives, never both. Each declares the same `owl:Ontology` IRIs,
    /// so scanning both would trip the duplicate-identity check and leave the server
    /// with no catalogue - a repair that broke exactly what it was fixing.
    pub fn release_in_use(&self) -> PathBuf {
        match usable(&self.release) {
            true => self.release.clone(),
            false => release::repair_dir(&self.user),
        }
    }

    /// Does the release need fetching? Only when neither directory is usable.
    pub fn needs_repair(&self) -> bool {
        !usable(&self.release) && !usable(&release::repair_dir(&self.user))
    }
}

/// Can this directory serve as the release half of the catalogue?
///
/// The three cases are genuinely different, and collapsing them is how a corrupt
/// install gets treated as an empty one (or vice versa):
///
///   * **A manifest that verifies** - a published release, intact. Use it.
///   * **No manifest, but ontologies present** - someone assembled this by hand, or a
///     test fixture. Not a release at all, so there is nothing to verify against and
///     nothing to repair; trust it, the same way the user root is trusted.
///   * **A manifest that does not match, or nothing at all** - a damaged or absent
///     release. A truncated artifact still parses and simply holds fewer terms, so
///     "some `.ttl.gz` are present" is not evidence of anything.
pub(super) fn usable(dir: &Path) -> bool {
    match release::verify(dir) {
        Ok(()) => true,
        Err(release::Invalid::NoManifest) => ontology_files(dir).is_ok_and(|f| !f.is_empty()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ontology")
            .join(name)
    }
    fn published_release(dir: &Path) {
        use std::io::Write;
        let content = "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
                           <http://example.org/r/X> a owl:Class .\n";
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(content.as_bytes()).unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("r.ttl.gz"), enc.finish().unwrap()).unwrap();
        let sha = hex::encode(Sha256::digest(content.as_bytes()));
        std::fs::write(
            dir.join("metadata.json"),
            format!(
                r#"{{"sources":[{{"artifact":{{"name":"r.ttl.gz","content_sha256":"{sha}"}}}}]}}"#
            ),
        )
        .unwrap();
    }

    fn roots_at(base: &Path) -> Roots {
        Roots {
            release: base.join("release"),
            user: base.join("user"),
        }
    }

    /// A hand-assembled directory is not a damaged release - there is no manifest to
    /// fail, nothing to repair, and re-downloading over it would destroy the install.
    #[test]
    fn directory_without_a_manifest_is_used_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_at(tmp.path());
        std::fs::create_dir_all(&roots.release).unwrap();
        std::fs::copy(
            fixture("share").join("mini.ttl"),
            roots.release.join("mini.ttl"),
        )
        .unwrap();

        assert!(!roots.needs_repair(), "no manifest is not a defect");
        assert_eq!(roots.release_in_use(), roots.release);
        assert!(roots.load().is_ok());
    }

    #[test]
    fn published_release_that_verifies_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_at(tmp.path());
        published_release(&roots.release);
        assert!(!roots.needs_repair());
        assert_eq!(roots.release_in_use(), roots.release);
    }

    /// The case the manifest exists for. A truncated artifact parses fine and simply
    /// holds fewer terms, so file-existence checks would accept it happily.
    #[test]
    fn release_whose_manifest_does_not_match_is_refused_and_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_at(tmp.path());
        published_release(&roots.release);
        // Same file, different content - exactly what a partial copy looks like.
        published_release(&roots.release.join("scratch"));
        std::fs::copy(
            roots.release.join("scratch").join("r.ttl.gz"),
            roots.release.join("r.ttl.gz"),
        )
        .unwrap();
        std::fs::write(roots.release.join("r.ttl.gz"), b"truncated").unwrap();

        assert!(roots.needs_repair(), "a mismatching manifest is a defect");
        assert_eq!(
            roots.release_in_use(),
            crate::memory::pkm::ontology::release::repair_dir(&roots.user),
            "falls back to where a repair would land"
        );
    }

    /// Once repaired, the repaired copy supplies the release and the broken image copy
    /// is not scanned at all - both declare the same ontology IRIs, so loading the two
    /// together would fail the duplicate-identity check.
    #[test]
    fn repaired_copy_replaces_the_broken_one_rather_than_joining_it() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots_at(tmp.path());
        std::fs::create_dir_all(&roots.release).unwrap();
        std::fs::write(roots.release.join("metadata.json"), r#"{"sources":[]}"#).unwrap();
        std::fs::copy(
            fixture("share").join("mini.ttl"),
            roots.release.join("mini.ttl"),
        )
        .unwrap();
        // An empty `sources` list verifies vacuously, so break it properly.
        std::fs::write(
            roots.release.join("metadata.json"),
            r#"{"sources":[{"artifact":{"name":"absent.ttl.gz","content_sha256":"00"}}]}"#,
        )
        .unwrap();

        let repair = crate::memory::pkm::ontology::release::repair_dir(&roots.user);
        published_release(&repair);

        assert!(!roots.needs_repair(), "the repaired copy is usable");
        assert_eq!(roots.release_in_use(), repair);
        let c = roots.load().expect("loads from the repaired copy alone");
        assert_eq!(
            c.sources().len(),
            1,
            "the broken copy is not scanned: {:?}",
            c.sources()
        );
    }
}
