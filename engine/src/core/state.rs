use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use metrics_exporter_prometheus::PrometheusHandle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::task::executor::TaskExecutor;

use crate::agent::service::AgentService;
use crate::agent::skill::resolver::SkillResolver;
use crate::agent::workspace::AgentWorkspaceManager;
use crate::auth::AuthService;
use crate::auth::jwt::JwtService;
use crate::auth::oauth::service::OAuthService;
use crate::auth::token::service::TokenService;
use crate::chat::broadcast::BroadcastService;
use crate::chat::service::ChatService;
use crate::credential::keypair::service::KeyPairService;
use crate::credential::service::CredentialService;
use crate::inference::ModelProviderRegistry;
use crate::inference::config::ModelRegistryConfig;
use crate::memory::service::MemoryService;
use crate::agent::prompt::PromptLoader;
use crate::space::service::SpaceService;
use crate::agent::task::service::TaskService;
use crate::tool::browser::session::BrowserSessionManager;
use crate::tool::cli::{CliToolConfig, load_cli_tool_configs};
use crate::tool::web_search::{SearchProvider, create_search_provider};
use crate::tool::workspace::WorkspaceManager;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

use super::config::Config;
use crate::api::repo::generic::SurrealRepo;
use crate::api::repo::users::SurrealUserRepo;

#[derive(Clone, Default)]
pub struct ActiveSessions {
    inner: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl ActiveSessions {
    pub async fn register(&self, chat_id: &str) -> CancellationToken {
        let mut map = self.inner.lock().await;
        if let Some(existing) = map.get(chat_id) {
            existing.cancel();
        }
        let token = CancellationToken::new();
        map.insert(chat_id.to_string(), token.clone());
        token
    }

    pub async fn cancel(&self, chat_id: &str) -> bool {
        let map = self.inner.lock().await;
        if let Some(token) = map.get(chat_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub async fn remove(&self, chat_id: &str) {
        self.inner.lock().await.remove(chat_id);
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Surreal<Db>,
    pub auth_service: Arc<AuthService>,
    pub user_repo: SurrealUserRepo,
    pub agent_service: AgentService,
    pub space_service: SpaceService,
    pub chat_service: ChatService,
    pub task_service: TaskService,
    pub credential_service: CredentialService,
    pub broadcast_service: BroadcastService,
    pub browser_session_manager: Arc<BrowserSessionManager>,
    pub active_sessions: ActiveSessions,
    pub memory_service: MemoryService,
    pub workspace_manager: Arc<WorkspaceManager>,
    pub cli_tools_config: Arc<Vec<CliToolConfig>>,
    pub search_provider: Option<Arc<dyn SearchProvider>>,
    pub skill_resolver: SkillResolver,
    pub task_executor: Arc<OnceLock<Arc<TaskExecutor>>>,
    pub max_concurrent_tasks: usize,
    pub config: Arc<Config>,
    pub agent_workspaces: AgentWorkspaceManager,
    pub prompts: PromptLoader,
    pub keypair_service: KeyPairService,
    pub token_service: TokenService,
    pub oauth_service: Option<OAuthService>,
    pub metrics_handle: PrometheusHandle,
}

impl AppState {
    pub fn new(
        db: Surreal<Db>,
        config: &Config,
        models_config: Option<ModelRegistryConfig>,
        workspaces: AgentWorkspaceManager,
        metrics_handle: PrometheusHandle,
    ) -> Self {
        let broadcast_service = BroadcastService::new();
        let llm_config = load_models_config(models_config);
        let provider_registry = ModelProviderRegistry::from_config(llm_config, broadcast_service.clone())
            .expect("Failed to initialize provider registry");

        let agent_repo = SurrealRepo::new(db.clone());
        let chat_repo = SurrealRepo::new(db.clone());
        let message_repo = SurrealRepo::new(db.clone());

        let workspace_manager = Arc::new(WorkspaceManager::new(
            &config.storage.workspaces_path,
            config.server.sandbox_disabled,
        ));
        let search_provider = create_search_provider(&config.search);

        let provider_registry_arc = Arc::new(provider_registry.clone());
        let prompt_loader = PromptLoader::new(PathBuf::from(&config.storage.shared_config_dir).join("prompts"));

        let cli_tools_config = load_cli_tool_configs(&prompt_loader);
        crate::tool::init_configurable_tools(&cli_tools_config);
        let cli_tools_config = Arc::new(cli_tools_config);

        let memory_service = MemoryService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            provider_registry_arc,
            prompt_loader.clone(),
            workspaces.clone(),
        );

        let skill_repo = SurrealRepo::new(db.clone());
        let skill_resolver = SkillResolver::new(skill_repo, &config.storage.shared_config_dir, workspaces.clone());

        let keypair_repo: SurrealRepo<crate::credential::keypair::models::KeyPair> =
            SurrealRepo::new(db.clone());
        let keypair_service = KeyPairService::new(
            &config.auth.encryption_secret,
            Arc::new(keypair_repo),
        );
        let jwt_service = JwtService::new();
        let token_repo: SurrealRepo<crate::auth::token::models::ApiToken> =
            SurrealRepo::new(db.clone());
        let token_service = TokenService::new(
            Arc::new(token_repo),
            jwt_service,
            config.auth.access_token_expiry_secs,
            config.auth.refresh_token_expiry_secs,
        );

        let oauth_service = if config.sso.enabled {
            let oauth_repo: SurrealRepo<crate::auth::oauth::models::OAuthIdentity> =
                SurrealRepo::new(db.clone());
            OAuthService::new(config, Arc::new(oauth_repo)).ok()
        } else {
            None
        };

        Self {
            db: db.clone(),
            auth_service: Arc::new(AuthService::new()),
            user_repo: SurrealRepo::new(db.clone()),
            agent_service: AgentService::new(SurrealRepo::new(db.clone())),
            space_service: SpaceService::new(SurrealRepo::new(db.clone())),
            chat_service: ChatService::new(
                chat_repo,
                message_repo,
                agent_repo,
                provider_registry,
                workspaces.clone(),
                memory_service.clone(),
                prompt_loader.clone(),
            ),
            task_service: TaskService::new(SurrealRepo::new(db.clone())),
            credential_service: CredentialService::new(SurrealRepo::new(db.clone())),
            broadcast_service: broadcast_service.clone(),
            browser_session_manager: Arc::new(BrowserSessionManager::new(config.browser.clone())),
            active_sessions: ActiveSessions::default(),
            memory_service,
            workspace_manager,
            cli_tools_config,
            search_provider,
            skill_resolver,
            task_executor: Arc::new(OnceLock::new()),
            max_concurrent_tasks: config.server.max_concurrent_tasks,
            config: Arc::new(config.clone()),
            agent_workspaces: workspaces,
            prompts: prompt_loader,
            keypair_service,
            token_service,
            oauth_service,
            metrics_handle,
        }
    }

    pub fn init_task_executor(&self) {
        let executor = TaskExecutor::new(self.clone());
        let _ = self.task_executor.set(Arc::new(executor));
    }

    pub fn task_executor(&self) -> Option<Arc<TaskExecutor>> {
        self.task_executor.get().cloned()
    }
}

fn load_models_config(from_yaml: Option<ModelRegistryConfig>) -> ModelRegistryConfig {
    match from_yaml {
        Some(mut config) => {
            config.merge_with_auto_discovered();
            tracing::info!("Loaded models config from config file");
            config
        }
        None => {
            tracing::info!("No models in config, auto-discovering from environment");
            ModelRegistryConfig::auto_discover()
        }
    }
}
