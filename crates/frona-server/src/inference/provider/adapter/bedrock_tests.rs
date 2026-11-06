use crate::{
    chat::broadcast::BroadcastService,
    core::{config::ModelProviderConfig, handle::Handle},
};
use frona_harness::inference::provider::InferenceCounter;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method},
};
fn counter() -> InferenceCounter {
    InferenceCounter::new(BroadcastService::new())
}
#[tokio::test]
async fn validation_proof_uses_the_exact_key_and_failed_candidates_never_stage() {
    use crate::inference::provider::validation::CandidateCredential;
    use crate::inference::provider::validation::ProviderValidationService;
    use crate::inference::provider::validation::ValidationCandidate;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Db;
    use surrealdb::engine::local::Mem;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message":"denied"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer valid-fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"asyncInvokeSummaries":[]})))
        .with_priority(1)
        .mount(&server)
        .await;
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    crate::db::init::setup_schema(&db).await.unwrap();
    let store =
        crate::credential::managed::test_support::credentials(db, "fixture-encryption").await;
    let service = ProviderValidationService::new(store.clone(), counter());
    let candidate = |key: &str| ValidationCandidate {
        handle: Handle::const_validated("bedrock-account"),
        config: ModelProviderConfig {
            provider: Some("amazon-bedrock".into()),
            aws_region: Some("eu-west-1".into()),
            base_url: Some(server.uri()),
            ..Default::default()
        },
        credential: CandidateCredential::New {
            method: frona_harness::config::CredentialMethod::ApiKey,
            document: crate::credential::managed::integration::static_secret::document(key.into()),
        },
    };
    assert!(service.validate(candidate("wrong-fixture")).await.is_err());
    assert!(store.vault().list().await.unwrap().is_empty());
    let proof = service.validate(candidate("valid-fixture")).await.unwrap();
    assert!(proof.models.is_none());
    assert!(store.vault().list().await.unwrap().is_empty());
}
