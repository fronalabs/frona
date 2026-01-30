use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::config::resolver::AgentConfigResolver;
use crate::agent::config::source::AgentConfigSource;
use crate::agent::prompt::resolver::PromptResolver;
use crate::agent::service::AgentService;
use crate::agent::skill::resolver::SkillResolver;
use crate::auth::AuthService;
use crate::chat::broadcast::BroadcastService;
use crate::chat::service::ChatService;
use crate::credential::service::CredentialService;
use crate::llm::ModelProviderRegistry;
use crate::llm::config::ModelRegistryConfig;
use crate::memory::service::MemoryService;
use crate::space::service::SpaceService;
use crate::task::service::TaskService;
use crate::tool::browser::config::BrowserConfig;
use crate::tool::browser::session::BrowserSessionManager;
use crate::tool::cli::{CliToolConfig, load_cli_tool_configs};
use crate::tool::web_search::{SearchProvider, create_search_provider};
use crate::tool::workspace::WorkspaceManager;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

use super::config::Config;
use super::repo::generic::SurrealRepo;
use super::repo::users::SurrealUserRepo;

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
}

impl AppState {
    pub fn new(db: Surreal<Db>, config: &Config) -> Self {
        let llm_config = load_models_config(&config.models_config_path);
        let provider_registry = ModelProviderRegistry::from_config(llm_config)
            .expect("Failed to initialize provider registry");

        let agent_repo = SurrealRepo::new(db.clone());
        let chat_repo = SurrealRepo::new(db.clone());
        let message_repo = SurrealRepo::new(db.clone());
        let prompt_repo = SurrealRepo::new(db.clone());

        let config_source = AgentConfigSource::load(&config.agents_config_dir);
        let config_resolver = AgentConfigResolver::new(config_source);
        let prompt_resolver = PromptResolver::new(prompt_repo, config_resolver);

        let browser_config = BrowserConfig {
            browserless_ws_url: config.browserless_ws_url.clone(),
            profiles_base_path: config.browser_profiles_path.clone(),
            connection_timeout_ms: 30000,
        };

        let cli_tools_config = load_cli_tool_configs(&config.tools_config_path);
        crate::tool::init_configurable_tools(&cli_tools_config);
        let cli_tools_config = Arc::new(cli_tools_config);
        let workspace_manager = Arc::new(WorkspaceManager::new(&config.workspaces_base_path));
        let search_provider = create_search_provider();

        let provider_registry_arc = Arc::new(provider_registry.clone());

        let memory_service = MemoryService::new(
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            SurrealRepo::new(db.clone()),
            provider_registry_arc,
        );

        let skill_repo = SurrealRepo::new(db.clone());
        let skill_resolver = SkillResolver::new(skill_repo, &config.skills_config_dir);

        Self {
            db: db.clone(),
            auth_service: Arc::new(AuthService::new(&config.jwt_secret)),
            user_repo: SurrealRepo::new(db.clone()),
            agent_service: AgentService::new(SurrealRepo::new(db.clone())),
            space_service: SpaceService::new(SurrealRepo::new(db.clone())),
            chat_service: ChatService::new(
                chat_repo,
                message_repo,
                agent_repo,
                provider_registry,
                prompt_resolver,
                memory_service.clone(),
            ),
            task_service: TaskService::new(SurrealRepo::new(db.clone())),
            credential_service: CredentialService::new(SurrealRepo::new(db.clone())),
            broadcast_service: BroadcastService::new(),
            browser_session_manager: Arc::new(BrowserSessionManager::new(browser_config)),
            active_sessions: ActiveSessions::default(),
            memory_service,
            workspace_manager,
            cli_tools_config,
            search_provider,
            skill_resolver,
        }
    }
}

fn load_models_config(path: &str) -> ModelRegistryConfig {
    match ModelRegistryConfig::load(path) {
        Ok(mut config) => {
            config.merge_with_auto_discovered();
            tracing::info!(path = %path, "Loaded models config");
            config
        }
        Err(e) => {
            tracing::info!(
                path = %path,
                error = %e,
                "No models config found, auto-discovering from environment"
            );
            ModelRegistryConfig::auto_discover()
        }
    }
}
