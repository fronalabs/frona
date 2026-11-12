use super::*;
use frona_pkm::ontology::SemanticSearchResult;
impl OntologyManager {
    pub(crate) async fn search_semantic_entities(
        &self,
        user_id: &str,
        query: &str,
        canonical_path: Option<&str>,
        class_limit: usize,
    ) -> Result<SemanticSearchResult, AppError> {
        let pass = self.cached_reason_user(user_id).await?;
        Ok(self.portable(user_id).await?.search_reasoned_entities(
            &pass,
            query,
            canonical_path,
            class_limit,
        )?)
    }
}
