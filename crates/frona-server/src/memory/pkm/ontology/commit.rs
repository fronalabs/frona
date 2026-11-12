use super::*;

impl OntologyManager {
    /// Cut and store this user's effective ontology, unconditionally.
    ///
    /// Run at the end of a pass, once reconcile and cleanup have settled which concepts
    /// survive. `load` remains correct without this call; this operation controls when the
    /// settled cut is stored.
    pub(crate) async fn save_effective_ontology(&self, user_id: &str) -> Result<(), AppError> {
        let Some(catalogue) = self.catalogue() else {
            return Ok(());
        };
        let row = self.repo.ontology_get(user_id).await?;
        let (ofn, version) = row
            .as_ref()
            .map(|ontology| (ontology.owl.clone(), ontology.version))
            .unwrap_or_else(|| (String::new(), 0));
        let delta_triples = schema::delta_triples(&ofn)?;
        let seeds = frona_pkm::ontology::seed_set(
            &catalogue,
            &delta_triples,
            &self.repo.ontology_terms(user_id).await?,
        );
        self.cut_and_store(user_id, &catalogue, row.as_ref(), seeds, version)
            .await?;
        Ok(())
    }

    /// One CAS attempt. Returns `Ok(None)` when the persisted version does not match.
    pub(super) async fn try_commit(
        &self,
        user_id: &str,
        edits: &[SchemaEdit],
        expected_version: i64,
    ) -> Result<Option<UserOntology>, AppError> {
        let current = self.load(user_id).await?;
        if current.version() != expected_version {
            return Ok(None);
        }
        let new_ofn = schema::apply_edits(current.delta_ofn(), edits, current.prefixes())?;
        match self
            .repo
            .ontology_upsert_cas(user_id, &new_ofn, DELTA_FORMAT, expected_version)
            .await?
        {
            Some(_) => Ok(Some(self.load(user_id).await?)),
            None => Ok(None),
        }
    }

    /// Apply schema edits and persist, retrying the reload and reapply loop on a CAS miss.
    pub async fn commit(&self, user_id: &str, edits: &[SchemaEdit]) -> Result<(), AppError> {
        if edits.is_empty() {
            self.load(user_id).await?;
            return Ok(());
        }
        for _ in 0..COMMIT_ATTEMPTS {
            let expected = self.load(user_id).await?.version();
            if self.try_commit(user_id, edits, expected).await?.is_some() {
                return Ok(());
            }
        }
        Err(AppError::Conflict(
            "ontology: commit exceeded CAS retry budget".into(),
        ))
    }
}
