//! CURIE ↔ IRI translation for the bundled vocabularies and the `frona:`
//! namespace. The storage model keeps entity `kind`, attribute keys, and link
//! relations as compact CURIEs (`schema:Person`, `foaf:name`, `frona:worksFor`);
//! the reasoner and SPARQL store want absolute IRIs. This is the single seam
//! between the two.

/// The prefixes bundled with the reference base. Ordered longest-namespace-first
/// so [`PrefixMap::compact`] picks the most specific match. `frona` sits last
/// among the `urn:` entries so an individual IRI (`urn:frona:kb:…`) is never
/// mistaken for a schema term (that namespace is guarded in `compact`).
pub const STANDARD_PREFIXES: &[(&str, &str)] = &[
    ("schema", "https://schema.org/"),
    ("foaf", "http://xmlns.com/foaf/0.1/"),
    ("skos", "http://www.w3.org/2004/02/skos/core#"),
    ("dcterms", "http://purl.org/dc/terms/"),
    // KBpedia: reference concepts and the upper structure (KKO) respectively. Bound
    // here rather than derived from the catalogue because a binding is only safe while
    // it is *fixed* - a CURIE written into a stored entity has to expand the same way
    // forever, so it cannot depend on which files happen to be present at load.
    ("kbpedia", "http://kbpedia.org/kko/rc/"),
    ("kko", "http://kbpedia.org/ontologies/kko#"),
    ("owl", "http://www.w3.org/2002/07/owl#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("frona", "urn:frona:"),
];

/// Namespace for ABox individuals (one per knowledge entity). Kept distinct from
/// the `frona:` term namespace so individuals and schema terms never collide.
pub const KB_NAMESPACE: &str = "urn:frona:kb:";

/// The default prefix a bare or unknown-prefixed term expands under.
const DEFAULT_PREFIX_NS: &str = "urn:frona:";

/// The prefix for per-user bespoke terms - the one namespace this knowledge base mints in,
/// and so the only one whose spelling it may normalise.
pub const FRONA_PREFIX: &str = "frona";

/// Which case convention a repaired `frona:` term takes. OWL does not require either, but
/// one term per concept does: `frona:solderingIron` and `frona:SolderingIron` are two
/// different terms, and nothing downstream can tell they were meant to be one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    /// `UpperCamelCase` - a class.
    Class,
    /// `lowerCamelCase` - an object or data property.
    Property,
}

/// A term a model sent that cannot be used, and why - see
/// [`PrefixMap::validate_term`]. `term` is kept verbatim so a rejection can quote back
/// exactly what was sent rather than a normalised version of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadTerm {
    pub term: String,
    pub reason: String,
}

impl std::fmt::Display for BadTerm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` cannot be used as a term: {}",
            self.term, self.reason
        )
    }
}

/// A resolver between compact CURIEs and absolute IRIs. Immutable; the bundled
/// prefixes cover every vocabulary in the base and all per-user `frona:` mints
/// (which reuse the `frona:` namespace), so there is no per-user prefix state.
#[derive(Debug, Clone)]
pub struct PrefixMap {
    prefixes: Vec<(String, String)>,
}

impl Default for PrefixMap {
    fn default() -> Self {
        Self::standard()
    }
}

impl PrefixMap {
    /// The bundled prefix set.
    pub fn standard() -> Self {
        Self {
            prefixes: STANDARD_PREFIXES
                .iter()
                .map(|(p, ns)| (p.to_string(), ns.to_string()))
                .collect(),
        }
    }

    #[cfg(test)]
    pub fn with_prefix(mut self, prefix: &str, namespace: &str) -> Self {
        self.prefixes.retain(|(current, _)| current != prefix);
        self.prefixes
            .push((prefix.to_string(), namespace.to_string()));
        self
    }

    /// The `(prefix, namespace)` pairs, for building a JSON-LD `@context`.
    pub fn entries(&self) -> &[(String, String)] {
        &self.prefixes
    }

    /// Expand a CURIE (`schema:Person`) to an absolute IRI. An already-absolute
    /// IRI (`http(s)://…`, `urn:…`) passes through unchanged; a bare token or an
    /// unknown prefix falls back to the `frona:` namespace.
    pub fn expand(&self, curie: &str) -> String {
        if curie.contains("://") || curie.starts_with("urn:") {
            return curie.to_string();
        }
        if let Some((pfx, local)) = curie.split_once(':') {
            for (p, ns) in &self.prefixes {
                if p == pfx {
                    return format!("{ns}{local}");
                }
            }
            // Unknown prefix - treat the whole thing as a frona local name so a
            // typo mints under our namespace rather than a bogus scheme.
            return format!("{DEFAULT_PREFIX_NS}{curie}");
        }
        format!("{DEFAULT_PREFIX_NS}{curie}")
    }

    /// Compact for display, falling back to the IRI when no prefix matches.
    ///
    /// The rendering half of "IRIs in the database, CURIEs in Markdown". Lossy on
    /// purpose - it is for frontmatter and prompts, never for anything read back.
    pub fn display(&self, iri: &str) -> String {
        self.compact(iri).unwrap_or_else(|| iri.to_string())
    }

    pub fn display_all(&self, iris: &[String]) -> Vec<String> {
        iris.iter().map(|i| self.display(i)).collect()
    }

    /// An entity's classes as the comma-separated CURIE list a model reads, and the form
    /// frontmatter renders. Duplicate classes are omitted.
    pub fn display_joined(&self, iris: &[String]) -> String {
        let mut seen = std::collections::HashSet::new();
        self.display_all(iris)
            .into_iter()
            .filter(|kind| seen.insert(kind.clone()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Is `curie` usable as a schema term - a CURIE this map can expand *and* compact
    /// back? `Err` carries the reason, phrased for whoever sent it (in practice a model).
    ///
    /// [`expand`](Self::expand) is deliberately total: it never fails, so an unknown
    /// prefix silently becomes a `frona:` local name and a term with a space in it
    /// silently becomes an IRI that no parser will read back. Both are permanent - entities
    /// are keyed by these strings - so the check has to happen where the term *enters*,
    /// which is the model boundary, not here at expansion time.
    ///
    /// The rule is legality, not style: `kbpedia:Doctor-Medical` and `foaf:mbox_sha1sum`
    /// are real terms in the bundled vocabularies, so hyphens and underscores pass. Only
    /// what genuinely cannot survive the round trip is refused. Style (camelCase, and
    /// which case for classes vs properties) is asked for in the prompts.
    pub fn validate_term(&self, curie: &str) -> Result<(), BadTerm> {
        let bad = |reason: String| {
            Err(BadTerm {
                term: curie.to_string(),
                reason,
            })
        };
        let t = curie.trim();
        if t.is_empty() {
            return bad("it is empty".into());
        }
        if t.len() != curie.len() {
            return bad("it has leading or trailing whitespace".into());
        }
        if t.contains("://") {
            return bad(format!(
                "it is a full IRI. Use a CURIE against one of the bound prefixes ({}) — a \
                 term outside them cannot be abbreviated, so it can never be recognised as \
                 the same term as anything else",
                self.prefix_list()
            ));
        }
        let Some((pfx, local)) = t.split_once(':') else {
            return bad(format!(
                "it has no prefix. Every term is `prefix:LocalName`, with the prefix one of \
                 {} — a bare word is silently treated as a `frona:` mint",
                self.prefix_list()
            ));
        };
        if !self.prefixes.iter().any(|(p, _)| p == pfx) {
            return bad(format!(
                "`{pfx}:` is not a bound prefix. Use one of {} — an unknown prefix is not an \
                 error at expansion time, it silently becomes part of a `frona:` term, so \
                 `{t}` would be minted as the bespoke term it looks like a typo for",
                self.prefix_list()
            ));
        }
        if local.is_empty() {
            return bad("it has a prefix but no local name after the colon".into());
        }
        if local.contains(':') {
            return bad(
                "its local name contains a second colon. The local name is one word — \
                 `frona:kb:` in particular is the namespace for entities, not for terms"
                    .into(),
            );
        }
        // Whitespace and the RFC 3987 delimiters: what an OWL/Turtle parser cannot read
        // back once written. A space here is the one that has actually happened.
        if let Some(c) = local
            .chars()
            .find(|c| c.is_whitespace() || c.is_control() || "<>\"{}|^`\\/".contains(*c))
        {
            return bad(format!(
                "its local name contains {}. Join the words by capitalising instead \
                 (`firmwareVersion`, not `firmware version`) — a term written this way \
                 produces an identifier no parser can read back, which takes the whole \
                 schema with it",
                if c.is_whitespace() {
                    "a space".to_string()
                } else {
                    format!("`{c}`")
                }
            ));
        }
        Ok(())
    }

    /// Repair a term into a usable CURIE, or say why it cannot be.
    ///
    /// Three repairs, in the order they apply:
    ///
    /// 1. **A full IRI becomes a CURIE**, when its namespace is bound -
    ///    `https://schema.org/worksFor` → `schema:worksFor`. Unbound, there is nothing to
    ///    repair: inventing a prefix would mint a term nothing else can match.
    /// 2. **A bare token gets the `frona:` prefix**, which is what `expand` was silently
    ///    assuming anyway. Making it explicit is the difference between a term that matches
    ///    itself later and one that does not.
    /// 3. **A `frona:` local name is restyled** to the house convention for its
    ///    [`TermKind`] - `support_email` → `supportEmail`, `Soldering Iron` →
    ///    `SolderingIron`.
    ///
    /// Restyling is confined to our own namespace on purpose. A standard term's spelling is
    /// not ours to normalise: `foaf:mbox_sha1sum` and `kbpedia:Doctor-Medical` are the terms
    /// those vocabularies actually declare, and "fixing" the case would produce a CURIE that
    /// expands to an IRI nothing declares. So outside `frona:` this only validates.
    pub fn repair_term(&self, raw: &str, kind: TermKind) -> Result<String, BadTerm> {
        use heck::{ToLowerCamelCase, ToUpperCamelCase};

        let bad = |reason: String| BadTerm {
            term: raw.to_string(),
            reason,
        };
        let mut t = raw.trim();
        if t.is_empty() {
            return Err(bad("it is empty".into()));
        }

        // (1) An absolute IRI is only repairable by compacting it.
        let compacted;
        if t.contains("://") || t.starts_with("urn:") {
            let Some(c) = self.compact(t) else {
                return Err(bad(format!(
                    "it is an IRI in no bound namespace, so it has no CURIE. Use a term \
                     under one of {} instead",
                    self.prefix_list()
                )));
            };
            compacted = c;
            t = &compacted;
        }

        // (2) A bare token is a `frona:` mint - the namespace `expand` already put it in.
        let (pfx, local) = match t.split_once(':') {
            Some((p, l)) => (p, l),
            None => (FRONA_PREFIX, t),
        };
        if !self.prefixes.iter().any(|(p, _)| p == pfx) {
            return Err(bad(format!(
                "`{pfx}:` is not a bound prefix, and guessing which one was meant would \
                 mint a different term than intended. Use one of {}",
                self.prefix_list()
            )));
        }

        // (3) Outside our namespace, validate only - see above.
        if pfx != FRONA_PREFIX {
            self.validate_term(t)?;
            return Ok(t.to_string());
        }
        let local = match kind {
            TermKind::Class => local.to_upper_camel_case(),
            TermKind::Property => local.to_lower_camel_case(),
        };
        if local.is_empty() {
            return Err(bad("it has no local name to repair".into()));
        }
        let out = format!("{FRONA_PREFIX}:{local}");
        // A repair that does not produce a usable term is a bug here, not bad input.
        self.validate_term(&out)?;
        Ok(out)
    }

    /// The bound prefixes as a model-readable list, for a rejection message.
    fn prefix_list(&self) -> String {
        self.prefixes
            .iter()
            .map(|(p, _)| format!("{p}:"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Compact an absolute IRI back to a CURIE, or `None` if no bundled prefix
    /// matches (or the IRI is an ABox individual, which is not a schema term).
    pub fn compact(&self, iri: &str) -> Option<String> {
        if iri.starts_with(KB_NAMESPACE) {
            return None;
        }
        for (p, ns) in &self.prefixes {
            if let Some(local) = iri.strip_prefix(ns.as_str()) {
                return Some(format!("{p}:{local}"));
            }
        }
        None
    }
}

/// The absolute IRI for the individual backing a knowledge entity at `path`.
pub fn individual_iri(path: &str) -> String {
    format!("{KB_NAMESPACE}{path}")
}

/// Recover an entity path from an individual IRI, or `None` if it is not one.
pub fn path_from_individual(iri: &str) -> Option<String> {
    iri.strip_prefix(KB_NAMESPACE).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_known_prefixes() {
        let m = PrefixMap::standard();
        assert_eq!(m.expand("schema:Person"), "https://schema.org/Person");
        assert_eq!(m.expand("foaf:name"), "http://xmlns.com/foaf/0.1/name");
        assert_eq!(m.expand("frona:worksFor"), "urn:frona:worksFor");
        assert_eq!(
            m.expand("xsd:integer"),
            "http://www.w3.org/2001/XMLSchema#integer"
        );
        assert_eq!(
            m.expand("kbpedia:Doctor-Medical"),
            "http://kbpedia.org/kko/rc/Doctor-Medical"
        );
        assert_eq!(
            m.expand("kko:Generals"),
            "http://kbpedia.org/ontologies/kko#Generals"
        );
    }

    /// The catalogue's two largest namespaces round-trip, or every KBpedia term a
    /// search offers is a full IRI the model then has to echo back verbatim.
    #[test]
    fn kbpedia_curies_round_trip() {
        let m = PrefixMap::standard();
        for curie in ["kbpedia:Doctor-Medical", "kko:Generals"] {
            assert_eq!(m.compact(&m.expand(curie)).as_deref(), Some(curie));
        }
    }

    #[test]
    fn expand_passes_through_absolute_and_defaults_bare() {
        let m = PrefixMap::standard();
        assert_eq!(
            m.expand("https://schema.org/Thing"),
            "https://schema.org/Thing"
        );
        assert_eq!(
            m.expand("urn:frona:kb:people/sarah"),
            "urn:frona:kb:people/sarah"
        );
        assert_eq!(m.expand("Database"), "urn:frona:Database");
        assert_eq!(m.expand("bogus:Term"), "urn:frona:bogus:Term");
    }

    #[test]
    fn compact_round_trips_and_guards_individuals() {
        let m = PrefixMap::standard();
        assert_eq!(
            m.compact("https://schema.org/Person").as_deref(),
            Some("schema:Person")
        );
        assert_eq!(
            m.compact("urn:frona:worksFor").as_deref(),
            Some("frona:worksFor")
        );
        assert_eq!(m.compact("urn:frona:kb:people/sarah"), None);
        assert_eq!(m.compact("http://example.com/x"), None);
    }

    /// Legality, not style: every one of these is a real term in a bundled vocabulary,
    /// so a stricter rule (camelCase only, alphanumeric only) would reject the catalogue.
    #[test]
    fn real_vocabulary_terms_validate() {
        let m = PrefixMap::standard();
        for t in [
            "schema:Person",
            "schema:worksFor",
            "foaf:mbox_sha1sum",      // underscore
            "kbpedia:Doctor-Medical", // hyphen
            "kko:Generals",
            "dcterms:title",
            "xsd:integer",
            "frona:firmwareVersion",
            "frona:SolderingIron",
        ] {
            assert!(
                m.validate_term(t).is_ok(),
                "`{t}` must validate: {:?}",
                m.validate_term(t)
            );
        }
    }

    /// The bug this exists for: `urn:frona:firmware download` was written into a delta,
    /// whose OFN then failed to parse on every subsequent read - so `apply_edits`,
    /// `test_edits` and `catalog` all threw and the user's schema layer stayed empty.
    #[test]
    fn local_name_with_a_space_is_refused() {
        let m = PrefixMap::standard();
        let e = m
            .validate_term("frona:firmware download")
            .expect_err("a space must be refused");
        assert_eq!(
            e.term, "frona:firmware download",
            "the term is quoted back verbatim"
        );
        assert!(
            e.reason.contains("a space"),
            "the reason names the character: {}",
            e.reason
        );
        assert!(
            e.reason.contains("firmwareVersion"),
            "and shows the fix: {}",
            e.reason
        );
    }

    /// An unbound prefix is the *silent* failure: `expand` turns `dc:title` into
    /// `urn:frona:dc:title`, a bespoke term that looks like a standard one.
    #[test]
    fn unbound_prefix_is_refused_and_says_which_are_bound() {
        let m = PrefixMap::standard();
        let e = m
            .validate_term("dc:title")
            .expect_err("`dc:` is not bound — `dcterms:` is");
        assert!(
            e.reason.contains("`dc:` is not a bound prefix"),
            "{}",
            e.reason
        );
        assert!(
            e.reason.contains("dcterms:"),
            "the list names the real one: {}",
            e.reason
        );
        assert_eq!(m.expand("dc:title"), "urn:frona:dc:title");
    }

    #[test]
    fn bare_word_a_full_iri_and_a_nested_colon_are_all_refused() {
        let m = PrefixMap::standard();
        assert!(
            m.validate_term("Database").is_err(),
            "a bare word has no prefix"
        );
        assert!(
            m.validate_term("https://ref.gs1.org/voc/manufacturer")
                .is_err(),
            "a full IRI outside the bound prefixes cannot compact back to a CURIE"
        );
        assert!(
            m.validate_term("frona:kb:people/me").is_err(),
            "that namespace holds entities"
        );
        assert!(
            m.validate_term("frona:").is_err(),
            "a prefix with no local name"
        );
        assert!(m.validate_term("").is_err());
        assert!(m.validate_term(" frona:port").is_err(), "untrimmed");
    }

    /// Whatever validates must survive expand → compact unchanged, since that round trip
    /// is what stored entities depend on. This is the property the character rule encodes.
    #[test]
    fn every_valid_term_round_trips() {
        let m = PrefixMap::standard();
        for t in [
            "schema:Person",
            "foaf:mbox_sha1sum",
            "kbpedia:Doctor-Medical",
            "frona:firmwareVersion",
            "xsd:integer",
        ] {
            m.validate_term(t).unwrap();
            assert_eq!(
                m.compact(&m.expand(t)).as_deref(),
                Some(t),
                "`{t}` must round-trip"
            );
        }
    }

    #[test]
    fn repair_fixes_case_bare_tokens_and_full_iris() {
        let m = PrefixMap::standard();
        let prop = |s: &str| m.repair_term(s, TermKind::Property).unwrap();
        let class = |s: &str| m.repair_term(s, TermKind::Class).unwrap();

        assert_eq!(prop("support_email"), "frona:supportEmail");
        assert_eq!(prop("frona:support_email"), "frona:supportEmail");
        assert_eq!(prop("frona:firmware download"), "frona:firmwareDownload");
        assert_eq!(
            prop("frona:firmwareversion"),
            "frona:firmwareversion",
            "one word, unknowable"
        );
        assert_eq!(prop("frona:FirmwareVersion"), "frona:firmwareVersion");
        assert_eq!(class("frona:soldering iron"), "frona:SolderingIron");
        assert_eq!(class("soldering-iron"), "frona:SolderingIron");

        assert_eq!(prop("forum_url"), "frona:forumUrl");
        assert_eq!(class("Database"), "frona:Database");

        assert_eq!(prop("https://schema.org/worksFor"), "schema:worksFor");
        assert_eq!(
            class("http://kbpedia.org/kko/rc/Doctor-Medical"),
            "kbpedia:Doctor-Medical"
        );
        assert_eq!(
            prop("urn:frona:firmware download"),
            "frona:firmwareDownload"
        );
    }

    /// A standard term is returned verbatim: `foaf:mbox_sha1sum` restyled would expand to an
    /// IRI FOAF does not declare, which is worse than the inconsistent case.
    #[test]
    fn repair_never_restyles_a_standard_term() {
        let m = PrefixMap::standard();
        for t in [
            "foaf:mbox_sha1sum",
            "kbpedia:Doctor-Medical",
            "schema:worksFor",
            "xsd:integer",
        ] {
            assert_eq!(
                m.repair_term(t, TermKind::Property).unwrap(),
                t,
                "`{t}` is not ours"
            );
        }
        assert_eq!(
            m.repair_term("schema:Person", TermKind::Class).unwrap(),
            "schema:Person",
            "and the kind does not override a standard spelling either"
        );
    }

    #[test]
    fn repair_refuses_what_it_would_have_to_guess() {
        let m = PrefixMap::standard();
        let e = m
            .repair_term("dc:title", TermKind::Property)
            .expect_err("`dc:` is unbound");
        assert!(e.reason.contains("guessing"), "{}", e.reason);
        assert!(
            m.repair_term("https://ref.gs1.org/voc/manufacturer", TermKind::Property)
                .is_err(),
            "an unbound namespace has no CURIE to compact to"
        );
        assert!(m.repair_term("", TermKind::Property).is_err());
        assert!(m.repair_term("frona:", TermKind::Property).is_err());
        assert!(
            m.repair_term("!!!", TermKind::Property).is_err(),
            "nothing left after slugging"
        );
    }

    /// The property repair is meant to be a fixed point: whatever it returns, feeding it
    /// back returns the same thing. Without that, two passes over one term give two terms.
    #[test]
    fn repair_is_idempotent() {
        let m = PrefixMap::standard();
        for raw in [
            "support_email",
            "frona:firmware download",
            "https://schema.org/worksFor",
            "foaf:mbox_sha1sum",
            "forum_url",
        ] {
            let once = m.repair_term(raw, TermKind::Property).unwrap();
            let twice = m.repair_term(&once, TermKind::Property).unwrap();
            assert_eq!(once, twice, "`{raw}` must settle after one repair");
            assert!(m.validate_term(&once).is_ok(), "and be usable: `{once}`");
        }
    }

    #[test]
    fn individual_iri_round_trips() {
        let iri = individual_iri("people/sarah");
        assert_eq!(iri, "urn:frona:kb:people/sarah");
        assert_eq!(path_from_individual(&iri).as_deref(), Some("people/sarah"));
        assert_eq!(path_from_individual("https://schema.org/Person"), None);
    }
}
