use super::super::{error::ApiError, middleware::auth::AdminUser};
use super::providers::{decode, service};
use crate::{
    core::{Handle, error::AppError, state::AppState},
    inference::provider::service::ModelListing,
};
use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    routing::get,
};
use serde_json::Value;

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ModelListingRequest {
    Validated(crate::inference::provider::service::DraftRequest),
    SavedCredential(crate::inference::provider::service::CredentialModelsRequest),
}

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/config/providers/{handle}/models",
        get(active_models).post(draft_models),
    )
}

async fn active_models(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    RawQuery(query): RawQuery,
) -> Result<Json<ModelListing>, ApiError> {
    let mut manual = Vec::new();
    if let Some(query) = query {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if key != "manual_model" {
                return Err(ApiError(AppError::Validation("model listing accepts only manual_model in URL queries; validate credentials and routing in a POST body".into())));
            }
            manual.push(value.into_owned());
        }
    }
    Ok(Json(
        service(&state)
            .active_models_with_manual(&handle, &manual)
            .await?,
    ))
}

async fn draft_models(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(handle): Path<Handle>,
    Json(body): Json<Value>,
) -> Result<Json<ModelListing>, ApiError> {
    let request: ModelListingRequest = decode(body)?;
    let service = service(&state);
    let listing = match request {
        ModelListingRequest::Validated(request) => {
            service
                .draft_models(&admin.0.user_id, &handle, request)
                .await?
        }
        ModelListingRequest::SavedCredential(request) => {
            service.credential_models(&handle, request).await?
        }
    };
    Ok(Json(listing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn saved_settings_model_listing_uses_credential_id_without_redaction_markers() {
        let credential_id = uuid::Uuid::new_v4();
        let mut settings = json!({ "providers": { "openai": {
            "provider": "openai", "credential_id": credential_id,
            "api_key": null, "base_url": null, "enabled": true,
        } } });
        crate::core::config::redact_config_for_api(&mut settings);
        let mut body = json!({ "config": settings["providers"]["openai"], "manual_models": [] });
        assert!(decode::<ModelListingRequest>(body.clone()).is_err());

        // The browser replaces the display-only marker before posting the config.
        body["config"]["api_key"] = Value::Null;
        let request =
            decode::<ModelListingRequest>(body).unwrap_or_else(|error| panic!("{}", error.0));
        let ModelListingRequest::SavedCredential(request) = request else {
            panic!("expected saved-credential model listing");
        };
        assert_eq!(request.config.credential_id, Some(credential_id));
        assert_eq!(request.config.api_key, None);
        assert!(request.manual_models.is_empty());
    }
}
