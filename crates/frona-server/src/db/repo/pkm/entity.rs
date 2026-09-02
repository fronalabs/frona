use super::*;

mod edit;
mod external;
mod projection;

#[derive(Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
struct RankedSearchRow {
    path: String,
    origin: EntityOrigin,
    category: EntityCategory,
    kinds: Vec<String>,
    name: String,
    description: String,
    aliases: std::collections::HashSet<String>,
    search_name_tokens: Vec<String>,
    search_assertions: Vec<String>,
    body: String,
    use_count: i64,
    score: f64,
}

impl From<RankedSearchRow> for RankedEntityHit {
    fn from(row: RankedSearchRow) -> Self {
        Self {
            entity: EntityHit {
                path: row.path,
                origin: row.origin,
                category: row.category,
                kinds: row.kinds,
                name: row.name,
                description: row.description,
                aliases: row.aliases,
                search_name_tokens: row.search_name_tokens,
                search_assertions: row.search_assertions,
                body: row.body,
            },
            score: row.score,
            use_count: row.use_count,
        }
    }
}

impl PkmRepo {
    pub(crate) fn search_top_k(&self) -> usize {
        self.search_top_k.max(1) as usize
    }

    pub async fn entity_by_path(
        &self,
        user_id: &str,
        path: &str,
    ) -> Result<Option<KnowledgeEntity>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity WHERE user_id = $uid AND path = $path LIMIT 1"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("path", path.to_string()))
            .await
            .map_err(|e| Self::err("entity_by_path", e))?;
        let rows: Vec<KnowledgeEntity> =
            q.take(0).map_err(|e| Self::err("page_by_path_take", e))?;
        Ok(rows.into_iter().next())
    }

    /// Delete an entity's record and its wikilink edges (`knowledge_entity_link`
    /// from/to). Memories and their `knowledge_entity_source` rows are **kept** -
    /// the memories stay canonical (marked erroneous by the caller), remain
    /// findable by `entity_path` for re-learn suppression, and are shielded from
    /// orphan GC. Used by the no-valid-memories entity GC.
    pub async fn delete_entity(&self, user_id: &str, path: &str) -> Result<(), AppError> {
        self.db
            .query(
                "BEGIN TRANSACTION;
                 DELETE knowledge_entity WHERE user_id = $uid AND path = $path;
                 DELETE knowledge_entity_link WHERE user_id = $uid
                     AND (from_entity_path = $path OR to_entity_path = $path);
                 COMMIT TRANSACTION",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("path", path.to_string()))
            .await
            .map_err(|e| Self::err("delete_page", e))?;
        Ok(())
    }

    /// Merge a mention into an entity that already exists - **aliases only**.
    ///
    /// Every other field on the skeleton has a later stage that owns it: the Classify stage
    /// assigns `kinds`, reconcile writes `name` and `description`, and `category` is
    /// fixed when the entity is created. Refreshing them from a mention would let extract
    /// overwrite work the pipeline has already done - the extractor is instance-blind
    /// (it re-emits every entity it sees), so a returning entity would be reset on every
    /// pass. `search_text` is re-derived from what the entity currently holds plus the new
    /// aliases, so the entity stays findable under both.
    async fn update_entity_skeleton(
        &self,
        existing: KnowledgeEntity,
        aliases: &[String],
    ) -> Result<(), AppError> {
        let mut merged = existing.aliases.clone();
        merged.extend(aliases.iter().cloned());
        let search_text = derive_search_text(&existing.name, &existing.description, &merged);
        let (search_names, search_name_tokens, mut search_assertions) = derive_resolution_search(
            &existing.name,
            &merged,
            &existing.attributes,
            std::iter::empty(),
        );
        search_assertions.extend(existing.search_assertions);
        search_assertions.sort();
        search_assertions.dedup();
        self.db
            .query(
                "UPDATE type::record('knowledge_entity', $id) SET
                    aliases = $aliases, search_text = $stext,
                    search_names = $search_names,
                    search_name_tokens = $search_name_tokens,
                    search_assertions = $search_assertions, updated_at = $now",
            )
            .bind(("id", existing.id))
            .bind(("aliases", merged))
            .bind(("stext", search_text))
            .bind(("search_names", search_names))
            .bind(("search_name_tokens", search_name_tokens))
            .bind(("search_assertions", search_assertions))
            .bind(("now", Utc::now()))
            .await
            .map_err(|e| Self::err("page_update", e))?;
        Ok(())
    }

    /// Create the entity if its path is new; else union the mention's aliases in.
    ///
    /// `category`, `kinds`, `name` and `description` are **create-only** - each is owned
    /// by a later stage, so an existing entity keeps what the pipeline gave it. See
    /// [`update_entity_skeleton`](Self::update_entity_skeleton). Attributes/use_count are
    /// owned elsewhere too, and were already excluded.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_entity_skeleton(
        &self,
        user_id: &str,
        path: &str,
        category: EntityCategory,
        kinds: &[String],
        name: &str,
        description: &str,
        aliases: &[String],
    ) -> Result<(), AppError> {
        for attempt in 0..CONFLICT_RETRIES {
            match self
                .try_upsert_entity_skeleton(
                    user_id,
                    path,
                    category,
                    kinds,
                    name,
                    description,
                    aliases,
                )
                .await
            {
                Err(error) if Self::is_write_conflict(&error) && attempt + 1 < CONFLICT_RETRIES => {
                    tokio::time::sleep(std::time::Duration::from_millis(5 << attempt)).await;
                }
                result => return result,
            }
        }
        unreachable!("the loop returns on its last attempt")
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_upsert_entity_skeleton(
        &self,
        user_id: &str,
        path: &str,
        category: EntityCategory,
        kinds: &[String],
        name: &str,
        description: &str,
        aliases: &[String],
    ) -> Result<(), AppError> {
        if let Some(existing) = self.entity_by_path(user_id, path).await? {
            return self.update_entity_skeleton(existing, aliases).await;
        } else {
            let now = Utc::now();
            let alias_set: std::collections::HashSet<String> = aliases.iter().cloned().collect();
            let search_text = derive_search_text(name, description, &alias_set);
            let (search_names, search_name_tokens, search_assertions) = derive_resolution_search(
                name,
                &alias_set,
                &serde_json::json!({}),
                std::iter::empty(),
            );
            let entity = KnowledgeEntity {
                id: new_id(),
                user_id: user_id.to_string(),
                path: path.to_string(),
                origin: EntityOrigin::Internal,
                category,
                kinds: kinds.to_vec(),
                name: name.to_string(),
                description: description.to_string(),
                identity_evidence: Vec::new(),
                attribute_sources: Vec::new(),
                source_memory_ids: Vec::new(),
                body: String::new(),
                sync_content: None,
                mirrored_rev: None,
                extracted_rev: None,
                related_playbooks: Vec::new(),
                search_text,
                search_names,
                search_name_tokens,
                search_assertions,
                attributes: serde_json::json!({}),
                use_count: 0,
                aliases: alias_set,
                rev: None,
                updated_at: now,
                rendered_at: chrono::DateTime::<Utc>::MIN_UTC,
            };
            let id = entity.id.clone();
            let created: Result<Option<surrealdb::types::Value>, _> = self
                .db
                .create(("knowledge_entity", id))
                .content(entity)
                .await;
            if created.is_err() {
                // `(user_id, path)` is UNIQUE, and this is a read-then-insert: a
                // concurrent writer may have created the entity between the two. Losing
                // that race is expected under parallel ingest, not an error - re-read
                // and merge into the winner.
                let Some(existing) = self.entity_by_path(user_id, path).await? else {
                    return created.map(|_| ()).map_err(|e| Self::err("page_insert", e));
                };
                return self.update_entity_skeleton(existing, aliases).await;
            }
        }
        Ok(())
    }

    /// The account owner's own `Person` entity (the reserved `SELF_ENTITY_PATH`), if it
    /// exists. Read by the `<user_profile>` injection.
    pub async fn self_entity(&self, user_id: &str) -> Result<Option<KnowledgeEntity>, AppError> {
        self.entity_by_path(user_id, SELF_ENTITY_PATH).await
    }

    /// Guarantee the owner's self-entity exists at the reserved `SELF_ENTITY_PATH`,
    /// creating it (display name from the `User` record) if absent. Returns the
    /// system-owned path.
    pub async fn ensure_self_entity(&self, user_id: &str, name: &str) -> Result<String, AppError> {
        if self.self_entity(user_id).await?.is_none() {
            let display = if name.trim().is_empty() {
                "the account owner"
            } else {
                name
            };
            self.upsert_entity_skeleton(
                user_id,
                SELF_ENTITY_PATH,
                EntityCategory::Concept,
                // Untyped, like any other new entity: the old seed was the bare string
                // "person", which expanded to `urn:frona:person` and carried no axioms.
                // Seeding a real class instead would be a behaviour change - the owner
                // entity would start clashing with anything disjoint from it - and that
                // is the Classify's call to make, not a default.
                &[],
                display,
                "The account owner.",
                &[],
            )
            .await?;
        }
        Ok(SELF_ENTITY_PATH.to_string())
    }

    /// Bump `updated_at` on every entity a memory is attached to.
    ///
    /// Retiring or re-homing a memory changes what its entities render, but touches only
    /// memory rows - so without this the affected entities carry no dirty signal and are
    /// invisible to `entities_needing_reconciliation`. Reconcile used to compensate by
    /// carrying the set in Rust, which worked inside a pass and was lost the moment one
    /// failed.
    pub(crate) async fn bump_entities_for_memory(
        &self,
        user_id: &str,
        memory_id: &str,
    ) -> Result<(), AppError> {
        self.db
            .query(
                "UPDATE knowledge_entity SET updated_at = $now WHERE user_id = $uid AND path IN
                 (SELECT VALUE entity_path FROM knowledge_entity_source
                  WHERE user_id = $uid AND memory_id = $mid)",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("mid", memory_id.to_string()))
            .bind(("now", Utc::now()))
            .await
            .map_err(|e| Self::err("bump_entities_for_memory", e))?;
        Ok(())
    }

    /// Union new aliases into an entity and re-derive its `search_text` so the resolver's
    /// self-healing write-back makes the entity findable under the new aliases.
    pub async fn add_entity_aliases(
        &self,
        user_id: &str,
        path: &str,
        new_aliases: &[String],
    ) -> Result<(), AppError> {
        let Some(existing) = self.entity_by_path(user_id, path).await? else {
            return Ok(());
        };
        let mut merged = existing.aliases.clone();
        merged.extend(new_aliases.iter().cloned());
        if merged.len() == existing.aliases.len() {
            return Ok(()); // nothing new
        }
        let search_text = derive_search_text(&existing.name, &existing.description, &merged);
        let (search_names, search_name_tokens, mut search_assertions) = derive_resolution_search(
            &existing.name,
            &merged,
            &existing.attributes,
            std::iter::empty(),
        );
        search_assertions.extend(existing.search_assertions);
        search_assertions.sort();
        search_assertions.dedup();
        self.db
            .query(
                "UPDATE type::record('knowledge_entity', $id) SET aliases = $aliases,
                     search_text = $stext, search_names = $search_names,
                     search_name_tokens = $search_name_tokens,
                     search_assertions = $search_assertions",
            )
            .bind(("id", existing.id))
            .bind(("aliases", merged))
            .bind(("stext", search_text))
            .bind(("search_names", search_names))
            .bind(("search_name_tokens", search_name_tokens))
            .bind(("search_assertions", search_assertions))
            .await
            .map_err(|e| Self::err("add_aliases", e))?;
        Ok(())
    }

    /// Move an entity to a new path: rewrite its memory links and entity-link edges,
    /// then the entity row itself - atomically, by delegating to the transactional
    /// [`rename_entities`](Self::rename_entities). Caller guarantees `to` is free.
    pub async fn rename_entity(&self, user_id: &str, from: &str, to: &str) -> Result<(), AppError> {
        self.rename_entities(user_id, &[(from.to_string(), to.to_string())])
            .await
    }

    /// Rename a batch of entity paths in a **single transaction** - each entity's row plus
    /// its memory-link and entity-link edges - so a directory rename commits
    /// all-or-nothing. Without this, a crash mid-batch leaves the DB split across
    /// old/new paths, and boot recovery (which reconciles files to whatever the DB
    /// holds) would cement the split rather than finish the move. Caller guarantees
    /// every `to` is free. No-op on an empty batch.
    pub async fn rename_entities(
        &self,
        user_id: &str,
        moves: &[(String, String)],
    ) -> Result<(), AppError> {
        if moves.is_empty() {
            return Ok(());
        }
        // One transaction over every entity's path move (record + memory-link + entity-link
        // edges), so it commits all-or-nothing. Each `tx.query` is its own statement, so
        // binds are per-entity - no shared-batch collisions. The handle has no
        // rollback-on-drop, so error paths must `cancel()` explicitly.
        let now = Utc::now();
        let tx = self
            .db
            .clone()
            .begin()
            .await
            .map_err(|e| Self::err("rename_pages_begin", e))?;
        for (from, to) in moves {
            let res = tx
                .query(
                    "UPDATE knowledge_entity_source SET entity_path = $to WHERE user_id = $uid AND entity_path = $from;
                     UPDATE knowledge_entity_link SET from_entity_path = $to WHERE user_id = $uid AND from_entity_path = $from;
                     UPDATE knowledge_entity_link SET to_entity_path = $to WHERE user_id = $uid AND to_entity_path = $from;
                     UPDATE knowledge_entity SET path = $to, updated_at = $now WHERE user_id = $uid AND path = $from",
                )
                .bind(("to", to.clone()))
                .bind(("from", from.clone()))
                .bind(("uid", user_id.to_string()))
                .bind(("now", now))
                .await;
            if let Err(e) = res {
                let _ = tx.cancel().await;
                return Err(Self::err("rename_entities", e));
            }
        }
        tx.commit()
            .await
            .map_err(|e| Self::err("rename_pages_commit", e))?;
        Ok(())
    }

    pub async fn entities_needing_reconciliation(
        &self,
        user_id: &str,
    ) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT VALUE path FROM knowledge_entity
                 WHERE user_id = $uid AND updated_at > rendered_at",
            )
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("needs_reconcile", e))?;
        q.take(0).map_err(|e| Self::err("needs_reconcile_take", e))
    }

    pub async fn entities_needing_reconciliation_by_category(
        &self,
        user_id: &str,
        category: EntityCategory,
    ) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT VALUE path FROM knowledge_entity
             WHERE user_id = $uid AND category = $category
               AND (path = 'people/me' OR path IN (
                   SELECT VALUE entity_path FROM knowledge_entity_source WHERE user_id = $uid
               ))
               AND updated_at > rendered_at",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("category", category))
            .await
            .map_err(|e| Self::err("needs_reconcile_category", e))?;
        q.take(0)
            .map_err(|e| Self::err("needs_reconcile_category_take", e))
    }

    /// Every Internal entity across all users - the canonical set `reconcile_files`
    /// renders at boot. Small (personal KB); no pagination needed.
    pub async fn all_internal_entities(&self) -> Result<Vec<KnowledgeEntity>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity WHERE origin = $origin"
            ))
            .bind(("origin", EntityOrigin::Internal))
            .await
            .map_err(|e| Self::err("all_internal_entities", e))?;
        q.take(0)
            .map_err(|e| Self::err("all_internal_entities_take", e))
    }

    /// Every entity for a user (any category/origin) - the ABox source for the
    /// ontology reasoning pass.
    pub async fn list_entities(&self, user_id: &str) -> Result<Vec<KnowledgeEntity>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity WHERE user_id = $uid"
            ))
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("list_entities", e))?;
        q.take(0).map_err(|e| Self::err("list_pages_take", e))
    }

    pub async fn list_all_entity_paths(&self, user_id: &str) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query("SELECT VALUE path FROM knowledge_entity WHERE user_id = $uid")
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("list_entities", e))?;
        q.take(0).map_err(|e| Self::err("list_pages_take", e))
    }

    /// All entities for a user in one category (e.g. `Playbook`) - one query, vs.
    /// fetching every path and re-reading each entity to filter.
    pub async fn list_entities_by_category(
        &self,
        user_id: &str,
        category: EntityCategory,
    ) -> Result<Vec<KnowledgeEntity>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_entity WHERE user_id = $uid AND category = $cat"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("cat", category))
            .await
            .map_err(|e| Self::err("list_entities_by_category", e))?;
        q.take(0)
            .map_err(|e| Self::err("list_pages_by_category_take", e))
    }

    /// BM25 over page metadata and body, scoped to the user. Metadata matches get
    /// triple weight because a matching name, description, or alias is stronger
    /// evidence than a mention in the body. `OR` semantics are required because
    /// `AND` gives near-zero recall on short fields. `use_count` breaks ties.
    pub async fn search_entities(
        &self,
        user_id: &str,
        query_text: &str,
    ) -> Result<Vec<EntityHit>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT path, origin, category, kinds, name, description, aliases, body,
                        search_name_tokens, search_assertions, use_count,
                        search::score(0) * 3 + search::score(1) AS score
                 FROM knowledge_entity
                 WHERE (search_text @0,OR@ $q OR body @1,OR@ $q) AND user_id = $uid
                 ORDER BY score DESC, use_count DESC LIMIT $k",
            )
            .bind(("q", query_text.to_string()))
            .bind(("uid", user_id.to_string()))
            .bind(("k", self.search_top_k))
            .await
            .map_err(|e| Self::err("page_fts", e))?;
        #[derive(Deserialize, Serialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct Raw {
            path: String,
            origin: EntityOrigin,
            category: EntityCategory,
            kinds: Vec<String>,
            name: String,
            description: String,
            aliases: std::collections::HashSet<String>,
            search_name_tokens: Vec<String>,
            search_assertions: Vec<String>,
            body: String,
        }
        let rows: Vec<Raw> = q.take(0).map_err(|e| Self::err("page_fts_take", e))?;
        Ok(rows
            .into_iter()
            .map(|r| EntityHit {
                path: r.path,
                origin: r.origin,
                category: r.category,
                kinds: r.kinds,
                name: r.name,
                description: r.description,
                aliases: r.aliases,
                search_name_tokens: r.search_name_tokens,
                search_assertions: r.search_assertions,
                body: r.body,
            })
            .collect())
    }

    pub(crate) async fn search_entity_metadata_candidates(
        &self,
        user_id: &str,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<RankedEntityHit>, AppError> {
        let mut response = self
            .db
            .query(
                "SELECT path, origin, category, kinds, name, description, aliases, body,
                        search_name_tokens, search_assertions, use_count,
                        search::score(0) AS score
                 FROM knowledge_entity
                 WHERE search_text @0,OR@ $q AND user_id = $uid
                 ORDER BY score DESC, use_count DESC, path ASC LIMIT $k",
            )
            .bind(("q", query_text.to_string()))
            .bind(("uid", user_id.to_string()))
            .bind(("k", limit.max(1) as i64))
            .await
            .map_err(|error| Self::err("entity_metadata_candidates", error))?;
        let rows: Vec<RankedSearchRow> = response
            .take(0)
            .map_err(|error| Self::err("entity_metadata_candidates_take", error))?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub(crate) async fn search_entity_body_candidates(
        &self,
        user_id: &str,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<RankedEntityHit>, AppError> {
        let mut response = self
            .db
            .query(
                "SELECT path, origin, category, kinds, name, description, aliases, body,
                        search_name_tokens, search_assertions, use_count,
                        search::score(0) AS score
                 FROM knowledge_entity
                 WHERE body @0,OR@ $q AND user_id = $uid
                 ORDER BY score DESC, use_count DESC, path ASC LIMIT $k",
            )
            .bind(("q", query_text.to_string()))
            .bind(("uid", user_id.to_string()))
            .bind(("k", limit.max(1) as i64))
            .await
            .map_err(|error| Self::err("entity_body_candidates", error))?;
        let rows: Vec<RankedSearchRow> = response
            .take(0)
            .map_err(|error| Self::err("entity_body_candidates_take", error))?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Separate lookup prevents body-only hits from displacing metadata candidates
    /// before the combined score is calculated.
    pub(crate) async fn search_entity_body_evidence_for_paths(
        &self,
        user_id: &str,
        query_text: &str,
        paths: &[String],
    ) -> Result<Vec<RankedEntityHit>, AppError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut response = self
            .db
            .query(
                "SELECT path, origin, category, kinds, name, description, aliases, body,
                        search_name_tokens, search_assertions, use_count,
                        search::score(0) AS score
                 FROM knowledge_entity
                 WHERE body @0,OR@ $q AND user_id = $uid AND path IN $paths
                 ORDER BY score DESC, use_count DESC, path ASC LIMIT $k",
            )
            .bind(("q", query_text.to_string()))
            .bind(("uid", user_id.to_string()))
            .bind(("paths", paths.to_vec()))
            .bind(("k", paths.len() as i64))
            .await
            .map_err(|error| Self::err("entity_body_evidence", error))?;
        let rows: Vec<RankedSearchRow> = response
            .take(0)
            .map_err(|error| Self::err("entity_body_evidence_take", error))?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Increment an entity's usefulness counter; returns the new count. Rejects
    /// unknown paths so the agent can't seed phantom boosts.
    pub async fn bump_entity_use(&self, user_id: &str, path: &str) -> Result<i64, AppError> {
        let Some(existing) = self.entity_by_path(user_id, path).await? else {
            return Err(AppError::Validation(format!("unknown entity path: {path}")));
        };
        let new = existing.use_count + 1;
        self.db
            .query("UPDATE type::record('knowledge_entity', $id) SET use_count = $c")
            .bind(("id", existing.id))
            .bind(("c", new))
            .await
            .map_err(|e| Self::err("bump_entity_use", e))?;
        Ok(new)
    }
}
