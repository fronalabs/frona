use super::*;

impl PkmRepo {
    /// Record a typed relation on the **subordinate** memory: it was superseded by
    /// `to` (global - the memory's role is entity-independent). Appends to `relations`.
    ///
    /// Retiring the subordinate changes what its entities render, so they are bumped dirty.
    pub async fn add_relation(
        &self,
        user_id: &str,
        subordinate_id: &str,
        relation: RelationType,
        to_id: &str,
        note: &str,
    ) -> Result<(), AppError> {
        let rel = MemoryRelation {
            relation,
            to: RecordId::new("knowledge_memory", to_id.to_string()),
            note: note.to_string(),
        };
        self.db
            .query("UPDATE type::record('knowledge_memory', $id) SET relations += [$rel]")
            .bind(("id", subordinate_id.to_string()))
            .bind(("rel", rel))
            .await
            .map_err(|e| Self::err("add_relation", e))?;
        self.bump_entities_for_memory(user_id, subordinate_id).await
    }

    /// Give an existing memory one more entity. Returns whether the link was new.
    ///
    /// `knowledge_entity_source` is many-to-many, and one fact routinely belongs to several
    /// entities: "Casey Owner works at Example Corp" is a fact about `people/me` **and** about
    /// `organizations/example-corp`. Extract links a memory to every entity it knew about at mining
    /// time; this is for the entity that did not exist yet - the Classify stage mints
    /// `organizations/example-corp` from an attribute value and the fact that stated it has to
    /// reach the new entity, or it is an entity with a name and nothing to say.
    ///
    /// Idempotent, because the Classify stage replays a banked classification on resume and
    /// would otherwise link the same fact twice.
    ///
    /// The bump matters as much as the link: the gained entity now renders differently, so
    /// it owes reconcile a description and author an article - the same signal
    /// [`union_memory_entities`](Self::union_memory_entities) raises for the survivor case.
    pub async fn attach_memory_to_entity(
        &self,
        user_id: &str,
        memory_id: &str,
        entity_path: &str,
    ) -> Result<bool, AppError> {
        if self
            .memory_entity_paths(user_id, memory_id)
            .await?
            .iter()
            .any(|p| p == entity_path)
        {
            return Ok(false);
        }
        let now = Utc::now();
        let link = KnowledgeEntitySource {
            id: new_id(),
            user_id: user_id.to_string(),
            memory_id: memory_id.to_string(),
            entity_path: entity_path.to_string(),
            created_at: now,
        };
        let res: Result<Option<surrealdb::types::Value>, _> = self
            .db
            .create(("knowledge_entity_source", link.id.clone()))
            .content(link)
            .await;
        if let Err(e) = res {
            return Err(Self::err("attach_memory_link", e));
        }
        self.db
            .query(
                "UPDATE knowledge_entity SET updated_at = $now
                 WHERE user_id = $uid AND path = $path",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("path", entity_path.to_string()))
            .bind(("now", now))
            .await
            .map_err(|e| Self::err("attach_memory_bump", e))?;
        Ok(true)
    }

    /// Build the entity-scoped view of two memories without changing either memory's entity
    /// memberships. Semantic equivalence does not imply that the survivor's exact wording
    /// applies to every entity the subordinate describes (for example, an assistant's
    /// `name` fact versus "Casey Owner named the assistant"). Reconcile uses this map to reject
    /// unsafe global retirement rather than turning coverage into subject corruption.
    pub async fn union_memory_entities(
        &self,
        user_id: &str,
        survivor_id: &str,
        from_id: &str,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
        let from_entitys = self.memory_entity_paths(user_id, from_id).await?;
        let survivor_pages = self.memory_entity_paths(user_id, survivor_id).await?;
        let mut by_page = std::collections::BTreeMap::<String, Vec<String>>::new();
        for p in survivor_pages {
            by_page.entry(p).or_default().push(survivor_id.to_string());
        }
        for p in from_entitys {
            by_page.entry(p).or_default().push(from_id.to_string());
        }
        for memories in by_page.values_mut() {
            memories.sort();
            memories.dedup();
        }
        Ok(by_page)
    }

    /// Live (current) memories on an entity - linked, no relation `relations`, disposition
    /// `None`. The reconcile feed: retired/settled memories never re-enter.
    pub async fn current_memories_for_entity(
        &self,
        user_id: &str,
        entity_path: &str,
    ) -> Result<Vec<KnowledgeMemory>, AppError> {
        let all = self.memories_for_entity(user_id, entity_path).await?;
        Ok(all
            .into_iter()
            .filter(|m| m.relations.is_empty() && m.disposition == Disposition::None)
            .collect())
    }

    /// Fetch durable memories referenced by a checkpoint projection before entity-source
    /// rows exist. The caller owns disposition filtering because historical evidence is
    /// intentionally available to classification.
    pub async fn memories_by_ids(
        &self,
        user_id: &str,
        memory_ids: &[String],
    ) -> Result<Vec<KnowledgeMemory>, AppError> {
        if memory_ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<RecordId> = memory_ids
            .iter()
            .map(|id| RecordId::new("knowledge_memory", id.clone()))
            .collect();
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_memory WHERE user_id = $uid AND id IN $ids"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("ids", ids))
            .await
            .map_err(|e| Self::err("memories_by_ids", e))?;
        let mut out: Vec<KnowledgeMemory> = q
            .take(0)
            .map_err(|e| Self::err("memories_by_ids_take", e))?;
        out.sort_by_key(|memory| std::cmp::Reverse(memory.created_at));
        Ok(out)
    }

    /// Memories attached to an entity path, newest-first.
    pub async fn memories_for_entity(
        &self,
        user_id: &str,
        entity_path: &str,
    ) -> Result<Vec<KnowledgeMemory>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT memory_id FROM knowledge_entity_source
                 WHERE user_id = $uid AND entity_path = $path",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("path", entity_path.to_string()))
            .await
            .map_err(|e| Self::err("memories_for_page_links", e))?;
        #[derive(Deserialize, Serialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct PageSourceRow {
            memory_id: String,
        }
        let sources: Vec<PageSourceRow> = q
            .take(0)
            .map_err(|e| Self::err("memories_for_page_take", e))?;
        let ids: Vec<RecordId> = sources
            .into_iter()
            .map(|l| RecordId::new("knowledge_memory", l.memory_id))
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_memory WHERE user_id = $uid AND id IN $ids"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("ids", ids))
            .await
            .map_err(|e| Self::err("memories_fetch", e))?;
        let mut out: Vec<KnowledgeMemory> =
            q.take(0).map_err(|e| Self::err("memories_fetch_take", e))?;
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(out)
    }

    /// Entity paths a memory is currently linked to.
    pub async fn memory_entity_paths(
        &self,
        user_id: &str,
        memory_id: &str,
    ) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT VALUE entity_path FROM knowledge_entity_source
                 WHERE user_id = $uid AND memory_id = $mid",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("mid", memory_id.to_string()))
            .await
            .map_err(|e| Self::err("memory_paths", e))?;
        q.take(0).map_err(|e| Self::err("memory_paths_take", e))
    }

    pub async fn list_all_memories(&self, user_id: &str) -> Result<Vec<KnowledgeMemory>, AppError> {
        let mut q = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_memory WHERE user_id = $uid"
            ))
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("list_memories", e))?;
        q.take(0).map_err(|e| Self::err("list_memories_take", e))
    }

    pub async fn delete_memory(&self, user_id: &str, memory_id: &str) -> Result<(), AppError> {
        // Memory + its links drop together (no dangling links / orphaned memory).
        self.db
            .query(
                "BEGIN TRANSACTION;
                 DELETE knowledge_entity_source WHERE user_id = $uid AND memory_id = $mid;
                 DELETE type::record('knowledge_memory', $mid);
                 COMMIT TRANSACTION",
            )
            .bind(("uid", user_id.to_string()))
            .bind(("mid", memory_id.to_string()))
            .await
            .map_err(|e| Self::err("delete_memory", e))?;
        Ok(())
    }

    /// Memory ids that are pure dead weight - retired via `Duplicate`/`Absorbed`
    /// only (dropped from *every* projection; their content is carried by the
    /// survivor). `Replace`/`Outdated` are kept (they render in History) and
    /// `Erroneous` is kept (re-learn suppression). Mirrors `classify_memories`'
    /// drop-class rule; computed in Rust to avoid enum-in-array SurrealQL.
    pub async fn dropped_memory_ids(&self, user_id: &str) -> Result<Vec<String>, AppError> {
        let memories = self.list_all_memories(user_id).await?;
        let mut dead = Vec::new();
        for m in memories {
            if m.disposition != Disposition::None {
                continue; // Outdated → History; Erroneous → kept for suppression
            }
            let has_drop = m
                .relations
                .iter()
                .any(|l| matches!(l.relation, RelationType::Duplicate | RelationType::Absorbed));
            let has_replace = m
                .relations
                .iter()
                .any(|l| l.relation == RelationType::Replace);
            if has_drop && !has_replace {
                dead.push(m.id);
            }
        }
        Ok(dead)
    }

    /// Entity paths that memories point at but where no entity row exists.
    ///
    /// `knowledge_entity_source` links by path string, not by a foreign key, so a memory
    /// can outlive its entity - a pass that committed memories and was then abandoned
    /// before its entities were created leaves exactly this. Its link row means it is not
    /// unreferenced, but nothing projects it until cleanup restores the entity.
    pub async fn dangling_memory_paths(&self, user_id: &str) -> Result<Vec<String>, AppError> {
        let mut q = self
            .db
            .query(
                "SELECT VALUE entity_path FROM knowledge_entity_source WHERE user_id = $uid;
                 SELECT VALUE path FROM knowledge_entity WHERE user_id = $uid",
            )
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("dangling_paths", e))?;
        let linked: Vec<String> = q.take(0).map_err(|e| Self::err("dangling_linked", e))?;
        let entities: std::collections::HashSet<String> = q
            .take::<Vec<String>>(1)
            .map_err(|e| Self::err("dangling_pages", e))?
            .into_iter()
            .collect();
        let mut out: Vec<String> = linked
            .into_iter()
            .filter(|p| !entities.contains(p))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Set a memory's disposition and stamp the matching timestamp (`ended_at`
    /// for `Outdated`, `erroneous_at` for `Erroneous`). The memory row itself is
    /// never deleted - disposition is how a fact is retired non-destructively.
    pub async fn set_disposition(
        &self,
        user_id: &str,
        memory_id: &str,
        disposition: Disposition,
    ) -> Result<(), AppError> {
        let now = Utc::now();
        let (ended_at, erroneous_at) = match disposition {
            Disposition::Outdated => (Some(now), None),
            Disposition::Erroneous => (None, Some(now)),
            Disposition::None | Disposition::Suspect => (None, None),
        };
        self.db
            .query(
                "UPDATE type::record('knowledge_memory', $id)
                 SET disposition = $disp, ended_at = $ended, erroneous_at = $err
                 WHERE user_id = $uid",
            )
            .bind(("id", memory_id.to_string()))
            .bind(("uid", user_id.to_string()))
            .bind(("disp", disposition))
            .bind(("ended", ended_at))
            .bind(("err", erroneous_at))
            .await
            .map_err(|e| Self::err("set_disposition", e))?;
        self.bump_entities_for_memory(user_id, memory_id).await
    }

    /// Mark **every** memory linked to an entity `Erroneous` (global - a memory
    /// shared onto other entities is retired there too, since deleting an entity means
    /// "these facts were wrong"). Used by the delete-Memory-entity path.
    pub async fn mark_entity_memories_erroneous(
        &self,
        user_id: &str,
        path: &str,
    ) -> Result<usize, AppError> {
        let memories = self.memories_for_entity(user_id, path).await?;
        let mut n = 0;
        for m in &memories {
            if m.disposition != Disposition::Erroneous {
                self.set_disposition(user_id, &m.id, Disposition::Erroneous)
                    .await?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Internal entity paths that HAD memories but now have **no valid**
    /// (non-erroneous) one left - the projection-side cleanup for unreferenced facts. An entity
    /// whose memories were all marked erroneous (e.g. the user deleted it)
    /// projects to nothing, so it's removed. A never-populated entity (zero
    /// memories) is left alone - it may be awaiting its first memory.
    pub async fn entities_with_no_valid_memories(
        &self,
        user_id: &str,
    ) -> Result<Vec<String>, AppError> {
        let paths = self.list_all_entity_paths(user_id).await?;
        let mut dead = Vec::new();
        for path in paths {
            let mems = self.memories_for_entity(user_id, &path).await?;
            if !mems.is_empty() && mems.iter().all(|m| m.disposition == Disposition::Erroneous) {
                dead.push(path);
            }
        }
        Ok(dead)
    }

    /// Normalized (trimmed + lowercased) contents of the `Erroneous` memories
    /// linked to an entity - the re-learn suppression set. A newly extracted fact
    /// matching one of these must not be re-minted (the user/agent retired it).
    pub async fn erroneous_contents_for_entity(
        &self,
        user_id: &str,
        path: &str,
    ) -> Result<std::collections::HashSet<String>, AppError> {
        let mems = self.memories_for_entity(user_id, path).await?;
        Ok(mems
            .into_iter()
            .filter(|m| m.disposition == Disposition::Erroneous)
            .map(|m| m.content.trim().to_lowercase())
            .collect())
    }
    /// Complete rejection snapshot, including paths whose entity rows were deleted.
    pub async fn erroneous_contents_snapshot(
        &self,
        user_id: &str,
    ) -> Result<std::collections::HashMap<String, std::collections::HashSet<String>>, AppError>
    {
        #[derive(Deserialize, Serialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct Source {
            entity_path: String,
            memory_id: String,
        }
        let mut query = self
            .db
            .query(
                "SELECT entity_path, memory_id FROM knowledge_entity_source WHERE user_id = $uid",
            )
            .bind(("uid", user_id.to_string()))
            .await
            .map_err(|e| Self::err("erroneous_snapshot_links", e))?;
        let sources: Vec<Source> = query
            .take(0)
            .map_err(|e| Self::err("erroneous_snapshot_links_take", e))?;
        let mut query = self
            .db
            .query(format!(
                "{SELECT} FROM knowledge_memory WHERE user_id = $uid AND disposition = $disposition"
            ))
            .bind(("uid", user_id.to_string()))
            .bind(("disposition", Disposition::Erroneous))
            .await
            .map_err(|e| Self::err("erroneous_snapshot_memories", e))?;
        let memories: Vec<KnowledgeMemory> = query
            .take(0)
            .map_err(|e| Self::err("erroneous_snapshot_memories_take", e))?;
        let contents: std::collections::HashMap<_, _> = memories
            .into_iter()
            .map(|memory| (memory.id, memory.content.trim().to_lowercase()))
            .collect();
        let mut out = std::collections::HashMap::<String, std::collections::HashSet<String>>::new();
        for source in sources {
            if let Some(content) = contents.get(&source.memory_id) {
                out.entry(source.entity_path)
                    .or_default()
                    .insert(content.clone());
            }
        }
        Ok(out)
    }
}
