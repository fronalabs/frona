use super::*;
use crate::core::{Handle, error::AppError};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

const ENV_PREFIX: &str = "FRONA_";

const EXCLUDED_ENV_VARS: &[&str] = &[
    "FRONA_CONFIG",
    "FRONA_LOG_CONFIG",
    "FRONA_LOG_LEVEL",
    "FRONA_SERVER_DATA_DIR",
];

pub struct LoadedConfig {
    pub(super) path: PathBuf,
    pub(super) revision: String,
    pub config: Config,
    pub models: Option<crate::inference::config::ModelRegistryConfig>,
}

impl ConfigService {
    /// Read a startup snapshot without a database. A missing file uses defaults
    /// and environment overrides; malformed or unreadable input is an error.
    pub fn load(path: impl AsRef<Path>) -> Result<LoadedConfig, AppError> {
        Self::load_with_env(path, std::env::vars().collect())
    }

    pub(super) fn load_with_env(
        path: impl AsRef<Path>,
        env: HashMap<String, String>,
    ) -> Result<LoadedConfig, AppError> {
        let data_dir = env
            .get("FRONA_SERVER_DATA_DIR")
            .cloned()
            .unwrap_or_else(|| "data".into());

        let config_path = path.as_ref().to_path_buf();
        let bytes = super::document::read_file(&config_path)?;
        let revision = super::document::revision(&bytes);
        let yaml_content = if bytes.is_empty() {
            None
        } else {
            Some(
                String::from_utf8(bytes)
                    .map_err(|error| AppError::Validation(error.to_string()))?,
            )
        };

        let mut builder = config::Config::builder()
            .set_default("database.path", format!("{data_dir}/db"))
            .unwrap()
            .set_default("storage.data_dir", data_dir.clone())
            .unwrap()
            .set_default("storage.skills_dir", format!("{data_dir}/skills"))
            .unwrap()
            .set_default("storage.cache_dir", format!("{data_dir}/system/cache"))
            .unwrap()
            .set_default("storage.ontology_dir", format!("{data_dir}/ontology"))
            .unwrap();

        if let Some(ref content) = yaml_content {
            let expanded = expand_config_env_vars(content)?;
            builder =
                builder.add_source(config::File::from_str(&expanded, config::FileFormat::Yaml));
        }

        // FRONA_BROWSER_WS_URL -> browser__ws_url -> browser.ws_url
        let frona_env: HashMap<String, String> = env
            .into_iter()
            .filter(|(k, _)| k.starts_with(ENV_PREFIX) && !EXCLUDED_ENV_VARS.contains(&k.as_str()))
            .map(|(k, v)| {
                let stripped = k[ENV_PREFIX.len()..].to_lowercase();
                let mapped = match stripped.find('_') {
                    Some(pos) => format!("{}__{}", &stripped[..pos], &stripped[pos + 1..]),
                    None => stripped,
                };
                (mapped, v)
            })
            .collect();

        builder = builder.add_source(
            config::Environment::default()
                .source(Some(frona_env))
                .separator("__")
                .try_parsing(true),
        );

        let built = builder
            .build()
            .map_err(|error| AppError::Validation(error.to_string()))?;

        let mut config: Config = built
            .try_deserialize()
            .map_err(|error| AppError::Validation(error.to_string()))?;

        // Preserve provider credential source references for runtime precedence.
        // Other configuration fields retain the existing expansion behavior.
        if let Some(content) = &yaml_content {
            let raw: serde_yaml::Value = serde_yaml::from_str(content)
                .map_err(|error| AppError::Validation(error.to_string()))?;
            if let Some(providers) = raw.get("providers").and_then(serde_yaml::Value::as_mapping) {
                for (name, provider) in providers {
                    let Some(name) = name.as_str() else { continue };
                    let Some(key) = provider.get("api_key").and_then(serde_yaml::Value::as_str)
                    else {
                        continue;
                    };
                    if key.starts_with("${") && key.ends_with('}') {
                        let handle = Handle::try_new(name)?;
                        if let Some(entry) = config.providers.get_mut(&handle) {
                            entry.api_key = Some(key.to_string());
                        }
                    }
                }
            }
        }

        resolve_server_timezone(&mut config.server);

        let models = if !config.models.is_empty() || !config.providers.is_empty() {
            Some(crate::inference::config::ModelRegistryConfig {
                providers: config.providers.clone().into_iter().collect(),
                models: config.models.clone().into_iter().collect(),
                skip_auto_discover: false,
            })
        } else {
            None
        };

        if yaml_content.is_some() {
            tracing::info!(path = %config_path.display(), "Loaded config from YAML");
        } else {
            tracing::info!("No config file found, using defaults and env vars");
        }

        if let Ok(mut v) = serde_json::to_value(&config) {
            redact_config_for_log(&mut v);
            tracing::debug!(
                "Effective config:\n{}",
                serde_json::to_string_pretty(&v).unwrap_or_default()
            );
        }

        Ok(LoadedConfig {
            config,
            models,
            path: config_path,
            revision,
        })
    }
}
