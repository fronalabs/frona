//! SPARQL execution over a materialized-closure store. The one place that runs a
//! query, so every caller (validation, inferred-link read-back, the Classify's
//! `ontology_sparql` tool) gets the bundled prefixes for free and shares the same
//! error mapping.

use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use oxrdf::Term;

use crate::Error;

use super::PrefixMap;

/// Build an evaluator with every bundled prefix registered, so callers may write
/// `schema:Person` / `frona:worksFor` without a `PREFIX` header.
fn evaluator(prefixes: &PrefixMap) -> Result<SparqlEvaluator, Error> {
    let mut ev = SparqlEvaluator::new();
    for (p, ns) in prefixes.entries() {
        ev = ev
            .with_prefix(p, ns)
            .map_err(|e| Error::Internal(format!("ontology: prefix {p}: {e}")))?;
    }
    Ok(ev)
}

/// Run a SPARQL query. A parse error is a `Validation` error (the query may be
/// LLM-authored); an evaluation error is `Internal`. The result is `'static`
/// because `on_store` clones the (Arc-backed) store into the bound query.
pub fn query(
    store: &Store,
    sparql: &str,
    prefixes: &PrefixMap,
) -> Result<QueryResults<'static>, Error> {
    evaluator(prefixes)?
        .parse_query(sparql)
        .map_err(|e| Error::Validation(format!("sparql parse error: {e}")))?
        .on_store(store)
        .execute()
        .map_err(|e| Error::Internal(format!("sparql eval error: {e}")))
}

pub fn ask(store: &Store, sparql: &str, prefixes: &PrefixMap) -> Result<bool, Error> {
    match query(store, sparql, prefixes)? {
        QueryResults::Boolean(b) => Ok(b),
        _ => Err(Error::Validation("expected an ASK query".into())),
    }
}

#[cfg(test)]
pub fn count(store: &Store, sparql: &str, var: &str, prefixes: &PrefixMap) -> Result<usize, Error> {
    match query(store, sparql, prefixes)? {
        QueryResults::Solutions(mut sols) => {
            let n = sols
                .next()
                .transpose()
                .map_err(|e| Error::Internal(format!("sparql solution: {e}")))?
                .and_then(|s| s.get(var).cloned())
                .and_then(term_as_usize)
                .unwrap_or(0);
            Ok(n)
        }
        _ => Err(Error::Validation("expected a SELECT query".into())),
    }
}

#[cfg(test)]
pub fn term_as_usize(term: Term) -> Option<usize> {
    match term {
        Term::Literal(l) => l.value().parse::<usize>().ok(),
        _ => None,
    }
}

pub fn term_lexical(term: &Term) -> String {
    match term {
        Term::Literal(l) => l.value().to_string(),
        Term::NamedNode(n) => n.as_str().to_string(),
        Term::BlankNode(b) => b.as_str().to_string(),
        #[allow(unreachable_patterns)]
        _ => term.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use oxrdf::{GraphName, NamedNode, Triple};

    use super::*;

    #[test]
    fn query_uses_a_nonbundled_authoritative_prefix_map() {
        let prefixes = PrefixMap::standard().with_prefix("example", "urn:example:");
        let store = Store::new().unwrap();
        let triple = Triple::new(
            NamedNode::new_unchecked("urn:example:alice"),
            NamedNode::new_unchecked("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
            NamedNode::new_unchecked("urn:example:Person"),
        )
        .in_graph(GraphName::DefaultGraph);
        store.insert(&triple).unwrap();

        assert_eq!(prefixes.expand("example:Person"), "urn:example:Person");
        assert_eq!(
            prefixes.compact("urn:example:Person").as_deref(),
            Some("example:Person")
        );
        assert_eq!(prefixes.display("urn:example:Person"), "example:Person");
        prefixes.validate_term("example:Person").unwrap();
        assert!(ask(&store, "ASK { example:alice a example:Person }", &prefixes,).unwrap());
    }
}
