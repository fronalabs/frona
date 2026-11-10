use std::{future::Future, pin::Pin, sync::Arc};

use super::{
    CredentialValidation, CredentialValidationError, ModelListError, ModelProvider,
    ProviderModelList, ProviderModelListSource,
};
use rig_core::model::ModelList;

type Check<T, E> =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<T, E>> + Send>> + Send + Sync>;

/// A connection's independent execution and account administration capabilities.
#[derive(Clone)]
pub struct ProviderConnection {
    pub model: Arc<dyn ModelProvider>,
    pub admin: ProviderAdmin,
}

/// Read-only inventory and credential probes, independent of model execution.
#[derive(Clone, Default)]
pub struct ProviderAdmin {
    listing: Option<Check<ProviderModelList, ModelListError>>,
    validation: Option<Check<CredentialValidation, CredentialValidationError>>,
}

impl ProviderAdmin {
    pub fn with_live_check<F, Fut>(self, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ModelList, CredentialValidationError>> + Send + 'static,
    {
        let check = Arc::new(check);
        let listing = check.clone();
        let mut admin = self.with_listing_check(move || {
            let check = listing.clone();
            async move {
                check()
                    .await
                    .map(|models| ProviderModelList::Listed {
                        source: ProviderModelListSource::Account,
                        models,
                    })
                    .map_err(|error| ModelListError(error.to_string()))
            }
        });
        admin.validation = Some(Arc::new(move || {
            let check = check.clone();
            Box::pin(async move {
                let models = check().await?;
                Ok(CredentialValidation {
                    models: Some(ProviderModelList::Listed {
                        source: ProviderModelListSource::Account,
                        models,
                    }),
                })
            })
        }));
        admin
    }

    pub fn with_listing_check<F, Fut>(mut self, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ProviderModelList, ModelListError>> + Send + 'static,
    {
        self.listing = Some(Arc::new(move || Box::pin(check())));
        self
    }

    pub fn with_validation<F, Fut>(mut self, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CredentialValidation, CredentialValidationError>>
            + Send
            + 'static,
    {
        self.validation = Some(Arc::new(move || Box::pin(check())));
        self
    }

    pub fn with_validation_check<F, Fut>(self, check: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), CredentialValidationError>> + Send + 'static,
    {
        self.with_validation(move || {
            let future = check();
            async move { future.await.map(|()| CredentialValidation { models: None }) }
        })
    }

    pub async fn list_models(&self) -> Result<ProviderModelList, ModelListError> {
        let check = self.listing.as_ref().ok_or_else(|| {
            ModelListError(
                "Live model discovery is unavailable for this adapter and authentication method"
                    .into(),
            )
        })?;
        check().await
    }

    pub async fn validate_credentials(
        &self,
    ) -> Result<CredentialValidation, CredentialValidationError> {
        self.validation
            .as_ref()
            .ok_or(CredentialValidationError::Unsupported)?()
        .await
    }
}
