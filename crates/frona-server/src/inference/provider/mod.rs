pub(crate) mod adapter;
pub(crate) mod registry;
pub mod service;
pub mod validation;

use crate::core::{Handle, config::ModelProviderConfig};
use frona_harness::inference::{
    InferenceError,
    provider::{
        InferenceCounter, ProviderConnection,
        platform::{ProviderPlatform, ResolvedConnection},
    },
};

/// Resolve an application account without passing its managed credential ID to the SDK.
pub fn resolve_connection(
    handle: &Handle,
    config: &ModelProviderConfig,
) -> Result<ResolvedConnection, InferenceError> {
    if config.credential_id.is_some()
        && (config.api_key.is_some()
            || config.azure_credential.is_some()
            || config.aws_profile.is_some())
    {
        return Err(InferenceError::ConfigError(
            "credential_id conflicts with another explicit authentication source".into(),
        ));
    }
    ProviderPlatform::resolve(&handle.clone().into(), &config.into())
}

pub(crate) fn build_provider(
    resolved: &ResolvedConnection,
    config: &ModelProviderConfig,
    counter: &InferenceCounter,
) -> Result<ProviderConnection, InferenceError> {
    if resolved.brand == "github-copilot" {
        Ok(adapter::copilot::SuppliedTokenProvider::new(
            config,
            resolved
                .effective_base_url
                .as_deref()
                .expect("Copilot recipe endpoint"),
            counter.clone(),
        )?
        .connection())
    } else {
        frona_harness::inference::provider::platform::build_provider(
            resolved,
            &config.into(),
            counter,
        )
    }
}
