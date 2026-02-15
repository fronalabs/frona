use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::agent::task::executor::TaskExecutor;

use crate::agent::service::AgentService;
use crate::agent::skill::resolver::SkillResolver;
use crate::agent::workspace::AgentWorkspaceManager;
use crate::auth::AuthService;
use crate::chat::broadcast::BroadcastService;
use crate::chat::service::ChatService;
use crate::credential::service::CredentialService;
use crate::inference::ModelProviderRegistry;
use crate::inference::config::ModelRegistryConfig;
use crate::memory::service::MemoryService;
use crate::agent::prompt::PromptLoader;
use crate::space::service::SpaceService;
use crate::agent::task::service::TaskService;
use crate::tool::browser::config::BrowserConfig;
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
}

impl AppState {
    pub fn new(db: Surreal<Db>, config: &Config, workspaces: AgentWorkspaceManager) -> Self {
        let broadcast_service = BroadcastService::new();
        let llm_config = load_models_config(&config.models_config_path);
        let provider_registry = ModelProviderRegistry::from_config(llm_config, broadcast_service.clone())
            .expect("Failed to initialize provider registry");

        let agent_repo = SurrealRepo::new(db.clone());
        let chat_repo = SurrealRepo::new(db.clone());
        let message_repo = SurrealRepo::new(db.clone());

        let browser_config = BrowserConfig {
            browserless_ws_url: config.browserless_ws_url.clone(),
            profiles_base_path: config.browser_profiles_path.clone(),
            connection_timeout_ms: 30000,
        };

        let workspace_manager = Arc::new(WorkspaceManager::new(
            &config.workspaces_base_path,
            config.sandbox_disabled,
        ));
        let search_provider = create_search_provider();

        let provider_registry_arc = Arc::new(provider_registry.clone());
        let prompt_loader = PromptLoader::new(PathBuf::from(&config.shared_config_dir).join("prompts"));

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
        let skill_resolver = SkillResolver::new(skill_repo, &config.shared_config_dir, workspaces.clone());

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
                workspaces.clone(),
                memory_service.clone(),
                prompt_loader.clone(),
            ),
            task_service: TaskService::new(SurrealRepo::new(db.clone())),
            credential_service: CredentialService::new(SurrealRepo::new(db.clone())),
            broadcast_service: broadcast_service.clone(),
            browser_session_manager: Arc::new(BrowserSessionManager::new(browser_config)),
            active_sessions: ActiveSessions::default(),
            memory_service,
            workspace_manager,
            cli_tools_config,
            search_provider,
            skill_resolver,
            task_executor: Arc::new(OnceLock::new()),
            max_concurrent_tasks: config.max_concurrent_tasks,
            config: Arc::new(config.clone()),
            agent_workspaces: workspaces,
            prompts: prompt_loader,
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
