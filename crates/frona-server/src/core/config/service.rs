use super::document::{read_file, revision, strip_defaults_against};
use serde::Serialize;
use serde_json::Value;
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

use crate::core::{
    Handle,
    config::{Config, merge_config_patch},
    error::AppError,
};
use crate::inference::{config::ModelRegistryConfig, provider::platform::ProviderPlatform};

#[derive(Clone)]
pub struct ConfigService {
    path: PathBuf,
    active_revision: String,
    active: Arc<Config>,
    defaults: Arc<Config>,
    writes: Arc<Mutex<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveFault {
    None,
    BeforeWrite,
    DuringWrite,
    AfterRename,
}

impl SaveFault {
    fn check(self, point: Self) -> Result<(), AppError> {
        if self == point {
            Err(AppError::Internal(
                "injected configuration save failure".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SaveResult {
    pub config: Value,
    pub persisted_revision: String,
    pub active_revision: String,
    pub restart_required: bool,
}

impl ConfigService {
    pub fn new(loaded: super::LoadedConfig) -> Result<Self, AppError> {
        if revision(&read_file(&loaded.path)?) != loaded.revision {
            return Err(AppError::Conflict(
                "configuration changed during startup; restart to load the new revision".into(),
            ));
        }
        Ok(Self {
            path: loaded.path,
            active_revision: loaded.revision,
            active: Arc::new(loaded.config),
            defaults: Arc::new(loaded.defaults),
            writes: Arc::new(Mutex::new(())),
        })
    }

    pub fn active(&self) -> Arc<Config> {
        self.active.clone()
    }

    pub fn persisted(&self) -> Result<SaveResult, AppError> {
        let bytes = read_file(&self.path)?;
        let persisted_revision = revision(&bytes);
        Ok(SaveResult {
            config: document(&bytes)?,
            restart_required: persisted_revision != self.active_revision,
            persisted_revision,
            active_revision: self.active_revision.clone(),
        })
    }

    pub async fn save(
        &self,
        patch: Value,
        expected_revision: Option<&str>,
    ) -> Result<SaveResult, AppError> {
        self.save_with_fault(patch, expected_revision, SaveFault::None)
            .await
    }

    async fn save_with_fault(
        &self,
        patch: Value,
        expected_revision: Option<&str>,
        fault: SaveFault,
    ) -> Result<SaveResult, AppError> {
        let _guard = self.writes.lock().await;
        if !patch.is_object()
            || [
                "validation_ids",
                "expected_persisted_revision",
                "persisted_revision",
                "active_revision",
            ]
            .iter()
            .any(|key| patch.get(key).is_some())
        {
            return Err(AppError::Validation(
                "configuration patch must be an object without request metadata".into(),
            ));
        }
        let bytes = read_file(&self.path)?;
        let previous_revision = revision(&bytes);
        if expected_revision.is_some_and(|expected| expected != previous_revision) {
            return Err(AppError::Conflict(
                "configuration revision changed; reload before saving".into(),
            ));
        }
        let mut target = document(&bytes)?;
        merge_config_patch(&mut target, patch)?;
        normalize_handles(&mut target)?;
        let config = validate_document(&target)?;
        for (handle, provider) in &config.providers {
            ProviderPlatform::resolve(handle, provider).map_err(validation)?;
        }
        ModelRegistryConfig {
            providers: config.providers,
            models: config.models,
            skip_auto_discover: true,
        }
        .validate_model_groups()
        .map_err(validation)?;

        strip_defaults_against(&mut target, &self.defaults);
        validate_document(&target)?;
        let yaml = serde_yaml::to_string(&target)
            .map_err(validation)?
            .into_bytes();
        fault.check(SaveFault::BeforeWrite)?;
        if revision(&read_file(&self.path)?) != previous_revision {
            return Err(AppError::Conflict(
                "configuration changed during save".into(),
            ));
        }
        atomic_write(&self.path, &yaml, fault)?;
        fault.check(SaveFault::AfterRename)?;
        let persisted_revision = revision(&yaml);
        Ok(SaveResult {
            config: target,
            restart_required: persisted_revision != self.active_revision,
            persisted_revision,
            active_revision: self.active_revision.clone(),
        })
    }
}

fn normalize_handles(value: &mut Value) -> Result<(), AppError> {
    if let Some(providers) = value.get_mut("providers").and_then(Value::as_object_mut) {
        let old = std::mem::take(providers);
        for (key, entry) in old {
            let handle = Handle::try_new(key)?;
            if providers.insert(handle.to_string(), entry).is_some() {
                return Err(AppError::Validation(
                    "provider handles collide after normalization".into(),
                ));
            }
        }
    }
    Ok(())
}

fn is_env_reference(value: &str) -> bool {
    value.starts_with("${") && value.ends_with('}')
}

pub(crate) fn validate_document(value: &Value) -> Result<Config, AppError> {
    // Validate expanded runtime values, but persist the untouched authoring data.
    // Provider key references stay references so database keys cannot override them.
    let yaml = serde_yaml::to_string(value).map_err(validation)?;
    let expanded = crate::core::config::expand_config_env_vars(&yaml)?;
    let mut config: Config = serde_yaml::from_str(&expanded).map_err(validation)?;
    if let Some(providers) = value.get("providers").and_then(Value::as_object) {
        for (name, provider) in providers {
            if let Some(key) = provider
                .get("api_key")
                .and_then(Value::as_str)
                .filter(|key| is_env_reference(key))
            {
                let handle = Handle::try_new(name)?;
                if let Some(entry) = config.providers.get_mut(&handle) {
                    entry.api_key = Some(key.into());
                }
            }
        }
    }
    Ok(config)
}

fn document(bytes: &[u8]) -> Result<Value, AppError> {
    if bytes.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_yaml::from_slice(bytes).map_err(validation)
}

fn atomic_write(path: &Path, bytes: &[u8], fault: SaveFault) -> Result<(), AppError> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(database)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(database)?;
    temporary
        .write_all(&bytes[..bytes.len() / 2])
        .map_err(database)?;
    fault.check(SaveFault::DuringWrite)?;
    temporary
        .write_all(&bytes[bytes.len() / 2..])
        .map_err(database)?;
    temporary.as_file().sync_all().map_err(database)?;
    temporary.persist(path).map_err(database)?;
    std::fs::File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(database)?;
    Ok(())
}

fn validation(error: impl std::fmt::Display) -> AppError {
    AppError::Validation(error.to_string())
}

fn database(error: impl std::fmt::Display) -> AppError {
    AppError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load(path: &Path) -> super::super::LoadedConfig {
        ConfigService::load_with_env(path, Default::default()).unwrap()
    }

    fn service(path: &Path) -> ConfigService {
        ConfigService::new(load(path)).unwrap()
    }

    async fn seed(service: &ConfigService) {
        service
            .save(
                json!({"providers":{"account":{"provider":"openai","api_key":"test-key"}}}),
                None,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn saves_strip_defaults_from_full_setup_sections() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        let mut submitted = serde_json::to_value(Config::default()).unwrap();
        submitted["auth"]["encryption_secret"] = json!("setup-test-secret");
        submitted["memory"]["backend"] = json!("basic");
        submitted["server"]["timezone"] = json!("America/Los_Angeles");
        submitted["providers"] = json!({"openai":{"provider":"openai","enabled":true,"credential_id":"00000000-0000-0000-0000-000000000001"}});
        submitted["models"] = json!({"primary":{"provider":"openai","model":"test","api":"completions","reasoning_effort":"medium",
            "extra_params":{"enabled":true,"temperature":null,"nested":{}}}});
        let expected = json!({
            "auth":{"encryption_secret":"setup-test-secret"},
            "memory":{"backend":"basic"},
            "server":{"timezone":"America/Los_Angeles"},
            "providers":{"openai":{"provider":"openai","credential_id":"00000000-0000-0000-0000-000000000001"}},
            "models":submitted["models"],
        });
        let saved = service.save(submitted.clone(), None).await.unwrap();
        assert_eq!(saved.config, expected);
        let written: Value =
            serde_yaml::from_slice(&std::fs::read(&service.path).unwrap()).unwrap();
        assert_eq!(written, expected);
        assert_eq!(
            serde_json::to_value(validate_document(&written).unwrap()).unwrap(),
            serde_json::to_value(validate_document(&submitted).unwrap()).unwrap()
        );
    }

    #[tokio::test]
    async fn saving_preserves_explicit_paths_with_environment_defaults() {
        for data_dir in ["data", "/srv/frona"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.yaml");
            let env = [("FRONA_SERVER_DATA_DIR".into(), data_dir.into())].into();
            let document = json!({
                "database": {"path": "data/db"},
                "storage": {
                    "data_dir": "data",
                    "skills_dir": "data/skills",
                    "cache_dir": "data/system/cache",
                    "ontology_dir": "data/ontology",
                },
            });
            std::fs::write(&path, serde_yaml::to_string(&document).unwrap()).unwrap();
            let loaded = ConfigService::load_with_env(&path, env).unwrap();
            let before = loaded.config.clone();
            let service = ConfigService::new(loaded).unwrap();
            let saved = service
                .save(json!({"server": {"port": 4321}}), None)
                .await
                .unwrap();
            let env = [("FRONA_SERVER_DATA_DIR".into(), data_dir.into())].into();
            let after = ConfigService::load_with_env(&path, env).unwrap().config;
            assert_eq!(after.database.path, before.database.path);
            assert_eq!(
                serde_json::to_value(&after.storage).unwrap(),
                serde_json::to_value(&before.storage).unwrap()
            );
            if data_dir == "data" {
                assert_eq!(saved.config, json!({"server": {"port": 4321}}));
            } else {
                assert_eq!(saved.config["database"], document["database"]);
                assert_eq!(saved.config["storage"], document["storage"]);
            }
        }
    }

    #[tokio::test]
    async fn saving_preserves_required_generic_model_provider() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        let service = service(&path);
        let saved = service
            .save(
                json!({
                    "providers": {"generic": {"provider": "openai"}},
                    "models": {"primary": {"provider": "generic", "model": "test"}},
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(saved.config["models"]["primary"]["provider"], "generic");
        let restarted = ConfigService::new(load(&path)).unwrap();
        assert_eq!(
            restarted.active().models["primary"].provider.as_str(),
            "generic"
        );
        restarted
            .save(json!({"server": {"port": 4321}}), None)
            .await
            .unwrap();
        assert_eq!(
            load(&path).config.models["primary"].provider.as_str(),
            "generic"
        );
    }

    #[tokio::test]
    async fn saving_a_default_removes_the_previous_override() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        service
            .save(json!({"server":{"port":4321}}), None)
            .await
            .unwrap();
        let saved = service
            .save(json!({"server":{"port":3001}}), None)
            .await
            .unwrap();
        assert_eq!(saved.config, json!({}));
        assert_eq!(load(&service.path).config.server.port, 3001);
    }

    #[tokio::test]
    async fn stripping_defaults_keeps_explicit_provider_connections() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        let saved = service
            .save(
                json!({
                    "providers":{"openai":{"enabled":true}},
                    "models":{"primary":{"provider":"openai","model":"test"}},
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(saved.config["providers"]["openai"], json!({}));
        assert!(load(&service.path).config.providers.contains_key("openai"));
    }

    #[tokio::test]
    async fn saves_only_change_the_persisted_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        let service = service(&path);
        let previous = service.persisted().unwrap();
        let saved = service
            .save(
                json!({"server":{"port":4321}}),
                Some(&previous.persisted_revision),
            )
            .await
            .unwrap();
        assert!(saved.restart_required);
        assert_eq!(saved.active_revision, previous.active_revision);
        assert_ne!(service.active().server.port, 4321);
        let restarted = ConfigService::new(load(&path)).unwrap();
        assert_eq!(restarted.active().server.port, 4321);
        assert!(!restarted.persisted().unwrap().restart_required);
    }

    #[tokio::test]
    async fn atomic_write_faults_preserve_an_entire_old_or_new_document() {
        for fault in [
            SaveFault::BeforeWrite,
            SaveFault::DuringWrite,
            SaveFault::AfterRename,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.yaml");
            let service = service(&path);
            service
                .save(json!({"server":{"port":4321}}), None)
                .await
                .unwrap();
            assert!(
                service
                    .save_with_fault(json!({"server":{"port":4322}}), None, fault)
                    .await
                    .is_err()
            );
            let persisted = service.persisted().unwrap();
            assert_eq!(
                persisted.config["server"]["port"],
                if fault == SaveFault::AfterRename {
                    4322
                } else {
                    4321
                }
            );
            // A manual edit after either crash point is authoritative.
            std::fs::write(&path, "server:\n  port: 4323\n").unwrap();
            let restarted = ConfigService::new(load(&path)).unwrap();
            assert_eq!(restarted.active().server.port, 4323);
        }
    }

    #[tokio::test]
    async fn concurrent_saves_and_external_edits_reject_stale_revisions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        let service = service(&path);
        let revision = service.persisted().unwrap().persisted_revision;
        let (a, b) = tokio::join!(
            service.save(json!({"server":{"port":4321}}), Some(&revision)),
            service.save(json!({"server":{"port":4322}}), Some(&revision))
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let previous = service.persisted().unwrap().persisted_revision;
        std::fs::write(&path, "server:\n  port: 4323\n").unwrap();
        assert!(service.save(json!({}), Some(&previous)).await.is_err());
        let loaded = load(&path);
        std::fs::write(&path, "server:\n  port: 4324\n").unwrap();
        assert!(ConfigService::new(loaded).is_err());
    }

    #[tokio::test]
    async fn explicit_ids_are_saved_without_database_activation() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        let id = uuid::Uuid::new_v4();
        let saved = service
            .save(
                json!({"providers":{"account":{"provider":"openai","credential_id":id}}}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            saved.config["providers"]["account"]["credential_id"],
            id.to_string()
        );
        assert!(
            service
                .save(
                    json!({"providers":{"account":{"api_key":"conflict"}}}),
                    None
                )
                .await
                .is_err()
        );
        service.save(json!({"providers":{"account":null,"renamed":{"provider":"openai","credential_id":id}}}), None).await.unwrap();
        assert_eq!(
            service.persisted().unwrap().config["providers"]["renamed"]["credential_id"],
            id.to_string()
        );
        service
            .save(json!({"providers":{"renamed":null}}), None)
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn raw_parameters_replace_preserve_clear_and_reload_without_interpretation() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        seed(&service).await;
        service
            .save(
                json!({"models":{"primary":{"provider":"account","model":"first",
                "extra_params":{"removed":1,"nested":{"old":true},"preserved":2}}}}),
                None,
            )
            .await
            .unwrap();
        let raw = json!({"nested":{"new":null},"array":[null,{"is_set":true},[1,2]],
                "reasoning.effort":"literal-key", "is_set":true, "template":"${FRONA_TEST_RAW_LITERAL_NEVER_EXPAND}",
                "auth":{"encryption_secret":{"is_set":true}}});
        let saved = service
            .save(json!({"models":{"primary":{"extra_params":raw}}}), None)
            .await
            .unwrap();
        assert_eq!(saved.config["models"]["primary"]["extra_params"], raw);
        let omitted = service
            .save(json!({"models":{"primary":{"temperature":0.5}}}), None)
            .await
            .unwrap();
        assert_eq!(omitted.config["models"]["primary"]["extra_params"], raw);
        let yaml = std::fs::read_to_string(&service.path).unwrap();
        let parsed: Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed["models"]["primary"]["extra_params"], raw);
        let expanded = crate::core::config::expand_config_env_vars(&yaml).unwrap();
        let reloaded: Config = serde_yaml::from_str(&expanded).unwrap();
        assert_eq!(
            Value::Object(reloaded.models["primary"].common.extra_params.clone()),
            raw
        );
        assert_eq!(
            service.persisted().unwrap().config["models"]["primary"]["extra_params"],
            raw
        );
        let before = std::fs::read(&service.path).unwrap();
        for invalid in [Value::Null, json!([]), json!("object required")] {
            assert!(
                service
                    .save(json!({"models":{"primary":{"extra_params":invalid}}}), None)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&service.path).unwrap(), before);
        }
        let marker = service
            .save(
                json!({"models":{"primary":{"extra_params":{"is_set":true}}}}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            marker.config["models"]["primary"]["extra_params"],
            json!({"is_set":true})
        );
        let empty = service
            .save(json!({"models":{"primary":{"extra_params":{}}}}), None)
            .await
            .unwrap();
        assert_eq!(empty.config["models"]["primary"]["extra_params"], json!({}));
    }

    #[tokio::test]
    async fn fallback_arrays_replace_completely_without_reusing_another_models_parameters() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        seed(&service).await;
        let a = json!({"provider":"account","model":"a","extra_params":{"owner":"a","null":null}});
        let b =
            json!({"provider":"account","model":"b","extra_params":{"owner":"b","is_set":true}});
        service.save(json!({"models":{"primary":{"provider":"account","model":"primary","extra_params":{"primary":true},"fallbacks":[a,b]}}}), None).await.unwrap();
        let moved = service
            .save(json!({"models":{"primary":{"fallbacks":[b,a]}}}), None)
            .await
            .unwrap();
        assert_eq!(
            moved.config["models"]["primary"]["fallbacks"],
            json!([b, a])
        );
        assert_eq!(
            moved.config["models"]["primary"]["extra_params"],
            json!({"primary":true})
        );
        let complete = json!({"provider":"account","model":"replacement"});
        let replaced = service
            .save(json!({"models":{"primary":{"fallbacks":[complete]}}}), None)
            .await
            .unwrap();
        assert!(
            replaced.config["models"]["primary"]["fallbacks"][0]
                .get("extra_params")
                .is_none()
        );
        assert!(service.save(json!({"models":{"primary":{"fallbacks":[{"provider":"account","extra_params":{}}]}}}), None).await.is_err());
        assert!(service.save(json!({"models":{"primary":{"fallbacks":[{"provider":"account","model":"test","extra_params":null}]}}}), None).await.is_err());
    }

    #[tokio::test]
    async fn redaction_markers_only_preserve_secrets_at_known_credential_paths() {
        let directory = tempfile::tempdir().unwrap();
        let service = service(&directory.path().join("config.yaml"));
        atomic_write(&service.path, b"auth:\n  encryption_secret: legacy-encryption\nproviders:\n  openai:\n    api_key: legacy-key\n", SaveFault::None).unwrap();
        let saved = service.save(json!({"auth":{"encryption_secret":{"is_set":true}}, "providers":{"openai":{"api_key":{"is_set":true}}},
                "models":{"primary":{"provider":"openai","model":"test","extra_params":{"providers":{"openai":{"api_key":{"is_set":true}}}}}}}), None).await.unwrap();
        assert_eq!(
            saved.config["auth"]["encryption_secret"],
            "legacy-encryption"
        );
        assert_eq!(saved.config["providers"]["openai"]["api_key"], "legacy-key");
        assert_eq!(
            saved.config["models"]["primary"]["extra_params"]["providers"]["openai"]["api_key"],
            json!({"is_set":true})
        );
    }
}
