use super::*;

/// A schema commit decided but not yet written - the delta's new text, the version it
/// was computed against (the CAS token), and its triples for judging entity types.
#[derive(Debug, Clone)]
pub struct PlannedSchema {
    pub owl: String,
    pub version: i64,
    pub triples: Vec<Triple>,
}

/// What stamping one class onto an entity comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypePlan {
    /// Write these kinds - the arrival was admitted (and the set re-normalised).
    Write(Vec<String>),
    /// The entity already carries the class; nothing to write, but it *is* typed.
    AlreadyHeld,
    /// Refused: it contradicts something the entity already has, or there is no catalogue.
    Refused,
}

impl Ontology {
    /// Reduce a set of classes to its most specific members.
    ///
    /// The model is asked not to return a class implied by another, and is not trusted
    /// to comply: `[Person, Thing]` says nothing `[Person]` does not, because the
    /// reasoner derives every ancestor anyway. Storing the implied one costs a triple
    /// on every pass, shows up in frontmatter, and invites the next Classify stage to think
    /// `Thing` was a deliberate judgement.
    ///
    /// Subsumption spans the catalogue **and** the user's delta. A class minted this
    /// pass - `frona:Engineer ⊑ schema:Person` - exists only in the delta, so a
    /// catalogue-only check would keep both and quietly defeat the whole exercise.
    ///
    /// Equivalent classes subsume each other, which would drop both. The first by sort
    /// order survives, so the outcome does not depend on what order the model listed
    /// them in.
    pub fn normalize_types(&self, kinds: &[String], delta: &[Triple]) -> Vec<String> {
        normalize_types(&self.catalogue, kinds, delta)
    }

    /// What an edit set *would* leave the schema as, without writing it.
    ///
    /// The pair that has to land together is the delta and the entity types stamped
    /// against it: an entity carrying a term the TBox does not declare is an entity the
    /// reasoner cannot make sense of. Planning first is what lets both go into one
    /// transaction - the reasoning needs reads the transaction should not hold open.
    pub async fn plan_schema(
        &self,
        user_id: &str,
        edits: &[SchemaEdit],
    ) -> Result<PlannedSchema, Error> {
        self.ensure_scope(user_id)?;
        let row = Some(self.state.clone());
        let version = row.as_ref().map(|o| o.version).unwrap_or(0);
        let owl = if edits.is_empty() {
            row.map(|o| o.owl).unwrap_or_default()
        } else {
            let current = self.load(user_id).await?;
            schema::apply_edits(current.delta_ofn(), edits, current.prefixes())?
        };
        let triples = schema::delta_triples(&owl)?;
        Ok(PlannedSchema {
            owl,
            version,
            triples,
        })
    }

    /// What adding `class` to an entity holding `kinds` would produce, judged against the
    /// schema `delta` that is about to be committed rather than the one on disk.
    ///
    /// Pure - the write is the caller's, so it can be batched with the schema commit.
    /// **The newest type loses**: everything already on the entity survived its own
    /// admission and has facts written against it, so the arrival is the only thing in
    /// question and the only thing refused.
    pub fn plan_entity_type(&self, kinds: &[String], class: &str, delta: &[Triple]) -> TypePlan {
        plan_entity_type(&self.catalogue, kinds, class, delta)
    }

    /// The entities an `align`/`merge` moves onto a new term, with the kinds each ends up
    /// with.
    ///
    /// Read-only, so the swap can be written in the same transaction as the schema that
    /// justifies it: the equivalence axiom alone would leave every entity from a previous
    /// pass sitting on the superseded term.
    pub async fn plan_retype(
        &self,
        user_id: &str,
        from_iri: &str,
        to_iri: &str,
        delta: &[Triple],
    ) -> Result<Vec<(String, Vec<String>)>, Error> {
        self.ensure_scope(user_id)?;
        let mut out = Vec::new();
        for entity in self.graph.knowledge_entities() {
            if !entity.kinds.iter().any(|k| k == from_iri) {
                continue;
            }
            let swapped: Vec<String> = entity
                .kinds
                .iter()
                .map(|k| {
                    if k == from_iri {
                        to_iri.to_string()
                    } else {
                        k.clone()
                    }
                })
                .collect();
            // Landing `to` on an entity that already carries something implying it would
            // otherwise leave both; normalising here keeps the stored set the most
            // specific one, so nothing downstream has to clean up after an alignment.
            out.push((entity.path, self.normalize_types(&swapped, delta)));
        }
        Ok(out)
    }
}

pub fn normalize_types(
    catalogue: &OntologyCatalogue,
    kinds: &[String],
    delta: &[Triple],
) -> Vec<String> {
    // `subClassOf` edges the delta adds on top of the catalogue.
    let mut delta_parents: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for t in delta {
        if t.predicate.as_str() != RDFS_SUBCLASS_OF {
            continue;
        }
        if let (NamedOrBlankNode::NamedNode(s), Term::NamedNode(o)) = (&t.subject, &t.object) {
            delta_parents
                .entry(s.as_str())
                .or_default()
                .push(o.as_str());
        }
    }

    let ancestors = |iri: &str| {
        crate::ontology::inspection::walk_ancestors(
            iri,
            |term| {
                catalogue
                    .direct_parents(term)
                    .into_iter()
                    .chain(
                        delta_parents
                            .get(term)
                            .into_iter()
                            .flatten()
                            .map(|parent| parent.to_string()),
                    )
                    .collect()
            },
            |term| catalogue.equivalents(term),
        )
    };

    // `KnowledgeEntity::kinds` is specified as full IRIs, but staged entities and old
    // records can still contain CURIEs. Canonicalise before equality or subsumption:
    // otherwise `schema:Person` and `https://schema.org/Person` survive as two
    // different strings and later render as the same type twice.
    let mut unique: Vec<String> = kinds
        .iter()
        .map(|kind| catalogue.prefixes().expand(kind))
        .collect();
    unique.sort();
    unique.dedup();
    let above: Vec<(String, std::collections::BTreeSet<String>)> =
        unique.iter().map(|k| (k.clone(), ancestors(k))).collect();

    let mut kept: Vec<String> = Vec::with_capacity(unique.len());
    for (i, (class, _)) in above.iter().enumerate() {
        let implied = above.iter().enumerate().any(|(j, (other, other_above))| {
            if i == j || !other_above.contains(class) {
                return false;
            }
            // Mutually subsuming - equivalent. Keep the earlier one only.
            let mutual = above[i].1.contains(other);
            !mutual || j < i
        });
        if !implied {
            kept.push(class.clone());
        }
    }
    // Preserve the caller's order among survivors - newest last is what makes
    // "reject the newest on a clash" meaningful - keeping first occurrences only.
    let mut seen = std::collections::HashSet::new();
    kinds
        .iter()
        .map(|kind| catalogue.prefixes().expand(kind))
        .filter(|kind| kept.contains(kind) && seen.insert(kind.clone()))
        .collect::<Vec<_>>()
}
pub fn plan_entity_type(
    catalogue: &OntologyCatalogue,
    kinds: &[String],
    class: &str,
    delta: &[Triple],
) -> TypePlan {
    let iri = catalogue.prefixes().expand(class);
    let current = normalize_types(catalogue, kinds, delta);
    if current.contains(&iri) {
        return if current == kinds {
            TypePlan::AlreadyHeld
        } else {
            TypePlan::Write(current)
        };
    }
    let mut proposed = current;
    proposed.push(iri.clone());
    // Normalised before the gate: a redundant arrival is dropped rather than
    // rejected, and an arrival that makes an existing type redundant retires it.
    // Nothing is lost either way - the reasoner derives every ancestor.
    let normalised = normalize_types(catalogue, &proposed, delta);
    if let Some(clash) = catalogue.clash(&normalised) {
        tracing::warn!(
            rejected = %iri,
            via = format!("{} ⊥ {}", clash.via.0, clash.via.1),
            "ontology: type rejected, it contradicts a type the entity already has"
        );
        return TypePlan::Refused;
    }
    TypePlan::Write(normalised)
}
