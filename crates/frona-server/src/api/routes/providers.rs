use super::super::error::ApiError;
use crate::{
    core::{error::AppError, state::AppState},
    inference::provider::service::ModelProviderService,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub(super) fn service(state: &AppState) -> &ModelProviderService {
    #[cfg(test)]
    PROVIDER_WORK.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    &state.model_provider_service
}

#[cfg(test)]
static PROVIDER_WORK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(super) fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| {
        ApiError(AppError::Validation(
            "invalid provider request; check the request fields and types".into(),
        ))
    })
}

