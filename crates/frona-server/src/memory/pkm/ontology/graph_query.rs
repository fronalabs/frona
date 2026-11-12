use super::*;
use frona_pkm::ontology::{GraphDirection, GraphEntity, GraphQueryOutput};
impl OntologyManager {
    pub(crate) async fn graph_entity(
        &self,
        user_id: &str,
        path: &str,
        direction: GraphDirection,
        relation: Option<&str>,
        edge_limit: usize,
    ) -> Result<Option<GraphEntity>, AppError> {
        Ok(self
            .cached_reason_user(user_id)
            .await?
            .graph_entity(path, direction, relation, edge_limit)?)
    }
    pub(crate) async fn query_graph(
        &self,
        user_id: &str,
        query: &str,
        row_limit: usize,
    ) -> Result<GraphQueryOutput, AppError> {
        Ok(self
            .cached_reason_user(user_id)
            .await?
            .query_graph(query, row_limit)?)
    }
}
