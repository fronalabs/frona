use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use frona_ontologies::graph::Graph;
use frona_ontologies::rdf::{
    P_ALT_LABEL, P_DISJOINT, P_EQ_CLASS, P_EQ_PROP, P_FIRST, P_REST, P_SUBCLASS, P_SUBPROP, P_TYPE,
    P_UNION,
};
use oxigraph::io::{RdfFormat, RdfParser};
use oxrdf::{NamedOrBlankNode, Term};

use crate::Error;

/// Every ontology file under `dir`, sorted for deterministic attribution. A missing
/// directory yields nothing rather than failing - the user root need not exist.
pub(super) fn ontology_files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Internal(format!(
                "ontology: read dir {}: {e}",
                dir.display()
            )));
        }
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| format_of(p).is_some())
        .collect();
    out.sort();
    Ok(out)
}

/// The RDF serialisation a path holds, `None` if it is not an ontology file at all
/// (`metadata.json`, `NOTICE`). `.gz` is stripped first, so `kbpedia.ttl.gz` is Turtle.
pub fn format_of(path: &Path) -> Option<RdfFormat> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".gz").unwrap_or(name);
    match stem.rsplit_once('.')?.1 {
        "ttl" => Some(RdfFormat::Turtle),
        "nt" => Some(RdfFormat::NTriples),
        "owl" | "rdf" => Some(RdfFormat::RdfXml),
        _ => None,
    }
}

/// `kbpedia.ttl.gz` → `kbpedia`.
pub(super) fn source_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    name.rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// Decompress if needed, and hand the parser a **stream**. Loading into a `Store`
/// first would build a fully-indexed copy just to iterate it once - 642 MB peak for
/// KBpedia against a 113 MB steady state.
pub(super) fn absorb(graph: &mut Graph, bytes: &[u8], path: &Path) -> Result<(), Error> {
    let fmt = format_of(path).expect("filtered by ontology_files");
    let reader: Box<dyn std::io::Read> = if is_gzip(path) {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else {
        Box::new(bytes)
    };
    graph
        .absorb_reader(reader, fmt)
        .map_err(|e| Error::Internal(format!("ontology: parse {}: {e}", path.display())))?;
    Ok(())
}

pub(super) fn is_gzip(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("gz")
}

/// What a file says about itself, as opposed to what it contributes to the graph.
pub(super) struct FileScan {
    /// The `owl:Ontology` it declares itself as, if any.
    pub(super) iri: Option<String>,
    /// Terms whose axioms are stated against an anonymous class expression.
    pub(super) anonymous: Vec<String>,
}

/// One streaming pass for the two facts the interned graph does not keep.
///
/// The graph models taxonomy, disjointness and equivalence and drops everything else -
/// which is exactly what makes its walk equal to OWL 2 RL, and also why the ontology
/// header and the shape of a dropped axiom are invisible in it. Both have to be read
/// here or not at all.
///
/// **Anonymous class expressions.** A blank node in the object position of
/// `subClassOf`, `equivalentClass` or friends. The graph silently drops the edge, so
/// the term ends up with no parent and every question about it returns a thin answer
/// with nothing reporting a problem. The one exception is
/// `X disjointWith [ owl:unionOf (A B C) ]` - how KBpedia states all 646 of its
/// disjointness axioms, and lossless once decomposed into `X⊥A, X⊥B, X⊥C`. A check
/// without that carve-out rejects the shipped catalogue.
///
/// Checked on **both** roots. The shipped release is held to the no-blank-node
/// contract by the pipeline's CI, but this pass has to run anyway for the header, so
/// exempting it would buy nothing and leave the guard untested against real input.
pub(super) fn scan_file(bytes: &[u8], path: &Path) -> Result<FileScan, Error> {
    const C_OWL_ONTOLOGY: &str = "http://www.w3.org/2002/07/owl#Ontology";
    let fmt = format_of(path).expect("filtered by ontology_files");
    let mut iri: Option<String> = None;
    let mut offenders: BTreeSet<String> = BTreeSet::new();
    // `unionOf` may be stated after the axiom referencing it, so disjointness is
    // collected and judged at the end rather than as it is read.
    let mut union_heads: HashSet<String> = HashSet::new();
    let mut disjoint_blanks: Vec<(String, String)> = Vec::new();

    let reader: Box<dyn std::io::Read> = if is_gzip(path) {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else {
        Box::new(bytes)
    };
    for q in RdfParser::from_format(fmt).for_reader(reader) {
        let q =
            q.map_err(|e| Error::Internal(format!("ontology: parse {}: {e}", path.display())))?;
        let subject = match &q.subject {
            NamedOrBlankNode::NamedNode(n) => n.as_str(),
            NamedOrBlankNode::BlankNode(b) => {
                if q.predicate.as_str() == P_UNION {
                    union_heads.insert(b.as_str().to_string());
                }
                continue;
            }
        };
        if q.predicate.as_str() == P_TYPE
            && matches!(&q.object, Term::NamedNode(o) if o.as_str() == C_OWL_ONTOLOGY)
        {
            // First declaration wins, which is document order and therefore stable.
            iri.get_or_insert_with(|| subject.to_string());
            continue;
        }
        let Term::BlankNode(o) = &q.object else {
            continue;
        };
        match q.predicate.as_str() {
            P_SUBCLASS | P_SUBPROP | P_EQ_CLASS | P_EQ_PROP => {
                offenders.insert(subject.to_string());
            }
            P_DISJOINT => disjoint_blanks.push((subject.to_string(), o.as_str().to_string())),
            // `rdf:first`/`rdf:rest` chain the union list itself and are expected.
            P_FIRST | P_REST | P_UNION | P_ALT_LABEL => {}
            _ => {}
        }
    }
    for (subject, blank) in disjoint_blanks {
        if !union_heads.contains(&blank) {
            offenders.insert(subject);
        }
    }
    Ok(FileScan {
        iri,
        anonymous: offenders.into_iter().collect(),
    })
}
