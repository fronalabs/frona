use super::*;

impl PkmRepo {
    /// Replace all **inferred** (`origin=Inferred`) entity links for a user with the
    /// reasoner's fresh output. Asserted edges are untouched; inferred edges are
    /// derived, so they are wiped and rewritten wholesale each reasoning pass.
    pub async fn wipe_inferred_links(&self, user_id: &str) -> Result<(), AppError> {
        self.db
            .query("DELETE knowledge_entity_link WHERE user_id = $uid AND origin = $origin")
            .bind(("uid", user_id.to_string()))
            .bind(("origin", LinkOrigin::Inferred))
            .await
            .map_err(|e| Self::err("wipe_inferred", e))?;
        Ok(())
    }

    /// Insert reasoner-materialized edges as `origin=Inferred`. Caller supplies
    /// `(from_entity_path, to_entity_path, relation)` already de-duplicated against the asserted
    /// set; typically paired with [`wipe_inferred_links`](Self::wipe_inferred_links)
    /// so the inferred set is a pure function of the current graph (idempotent).
    pub async fn insert_inferred_links(
        &self,
        user_id: &str,
        links: &[(String, String, String)],
    ) -> Result<(), AppError> {
        if links.is_empty() {
            return Ok(());
        }
        let now = Utc::now();
        for (from, to, rel) in links {
            let link = KnowledgeEntityLink {
                id: new_id(),
                user_id: user_id.to_string(),
                from_entity_path: from.clone(),
                to_entity_path: to.clone(),
                relation: rel.clone(),
                source_memory_ids: Vec::new(),
                origin: LinkOrigin::Inferred,
                created_at: now,
            };
            let _: Option<surrealdb::types::Value> = self
                .db
                .create(("knowledge_entity_link", link.id.clone()))
                .content(link)
                .await
                .map_err(|e| Self::err("insert_inferred", e))?;
        }
        Ok(())
    }

    /// Ontology seed terms referenced by entity kinds, attribute keys, asserted links,
    /// and built-in concept metadata. Stored IRIs and CURIEs remain unnormalized.
    pub async fn ontology_terms(&self, user_id: &str) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query(
                // `type::is_object` guards the keys call. `object::keys` is a hard
                // error on anything that is not an object, and rows predating the
                // `attributes` field have none - so a single such entity failed the
                // whole seed set, which took down every ontology load for that user.
                "SELECT VALUE kinds FROM knowledge_entity WHERE user_id = $uid;
                 SELECT VALUE object::keys(attributes) FROM knowledge_entity \
                     WHERE user_id = $uid AND type::is_object(attributes);
                 SELECT VALUE relation FROM knowledge_entity_link \
                     WHERE user_id = $uid AND origin != $inferred;
                 SELECT VALUE path FROM knowledge_entity \
                     WHERE user_id = $uid AND category = $concept LIMIT 1;",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("inferred", LinkOrigin::Inferred))
            .bind(("concept", EntityCategory::Concept))
            .await
            .map_err(|e| Self::err("ontology_terms", e))?;

        let kinds: Vec<Vec<String>> = q
            .take(0)
            .map_err(|e| Self::err("ontology_terms_kinds", e))?;
        let keys: Vec<Vec<String>> = q.take(1).map_err(|e| Self::err("ontology_terms_keys", e))?;
        let relations: Vec<String> = q
            .take(2)
            .map_err(|e| Self::err("ontology_terms_relations", e))?;
        let concepts: Vec<String> = q
            .take(3)
            .map_err(|e| Self::err("ontology_terms_concepts", e))?;

        let mut out: Vec<String> = kinds
            .into_iter()
            .flatten()
            .chain(keys.into_iter().flatten())
            .chain(relations)
            .filter(|t| !t.trim().is_empty())
            .collect();
        if !concepts.is_empty() {
            out.extend([
                ENTITY_NAME_PROPERTY_IRI.to_string(),
                ENTITY_PATH_PROPERTY_IRI.to_string(),
            ]);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    pub async fn asserted_links(
        &self,
        user_id: &str,
    ) -> Result<Vec<KnowledgeEntityLink>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity_link WHERE user_id = $uid AND origin != $inferred"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("inferred", LinkOrigin::Inferred))
            .await
            .map_err(|e| Self::err("asserted_links", e))?;
        q.take(0).map_err(|e| Self::err("asserted_links_take", e))
    }

    /// Complete entity graph for a user, including asserted and reasoner-materialized edges.
    pub async fn list_entity_links(
        &self,
        user_id: &str,
    ) -> Result<Vec<KnowledgeEntityLink>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity_link WHERE user_id = $uid"
            ))
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("list_entity_links", e))?;
        q.take(0)
            .map_err(|e| Self::err("list_entity_links_take", e))
    }

    pub async fn list_entity_sources(
        &self,
        user_id: &str,
    ) -> Result<Vec<KnowledgeEntitySource>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity_source WHERE user_id = $uid"
            ))
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("list_entity_sources", e))?;
        q.take(0)
            .map_err(|e| Self::err("list_entity_sources_take", e))
    }

    /// Outgoing edges from an entity (for frontmatter `[[wikilinks]]`).
    pub async fn links_from_entity(
        &self,
        user_id: &str,
        from_entity_path: &str,
    ) -> Result<Vec<KnowledgeEntityLink>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity_link WHERE user_id = $uid AND from_entity_path = $from"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("from", from_entity_path.to_string()))
            .await
            .map_err(|e| Self::err("links_from_entity", e))?;
        q.take(0)
            .map_err(|e| Self::err("links_from_entity_take", e))
    }

    /// Paths of entities that link **to** `to_entity_path` - i.e. whose rendered files contain a
    /// `[[to_entity_path]]` wikilink. Used by the complete-rename to rewrite those files.
    pub async fn entities_linking_to(
        &self,
        user_id: &str,
        to_entity_path: &str,
    ) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT VALUE from_entity_path FROM knowledge_entity_link WHERE user_id = $uid AND to_entity_path = $to",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("to", to_entity_path.to_string()))
            .await
            .map_err(|e| Self::err("entities_linking_to", e))?;
        q.take(0).map_err(|e| Self::err("pages_linking_to_take", e))
    }
}
