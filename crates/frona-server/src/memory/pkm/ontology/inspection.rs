use super::*;
use frona_pkm::ontology::OntologyExport;
impl OntologyManager {
    pub async fn catalog(&self, user_id: &str) -> Result<super::Catalog, AppError> {
        Ok(self.portable(user_id).await?.catalog(user_id).await?)
    }

    pub async fn export(&self, user_id: &str) -> Result<OntologyExport, AppError> {
        Ok(self.portable(user_id).await?.export(user_id).await?)
    }
}
