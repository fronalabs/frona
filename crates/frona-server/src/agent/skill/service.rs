use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::config::parse_frontmatter;
use crate::core::config::CacheConfig;
use crate::core::error::AppError;
use crate::storage::StorageService;

use super::registry::{self, SkillRegistryClient};
use super::resolver::{Skill, SkillResolver, SkillScope};

#[derive(Debug, Clone, Serialize)]
pub struct SkillListItem {
    pub name: String,
    pub description: String,
    pub source: Option<String>,
    pub installed_at: Option<DateTime<Utc>>,
    pub scope: SkillScope,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSearchResult {
    pub name: String,
    pub repo: String,
    pub avatar_url: String,
    pub installs: u64,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoBrowseSkill {
    pub name: String,
    pub description: String,
    pub sha: String,
    pub dir_path: String,
    pub installed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoBrowseResult {
    pub repo: String,
    pub description: String,
    pub avatar_url: String,
    pub skills: Vec<RepoBrowseSkill>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillPreview {
    pub name: String,
    pub description: String,
    pub body: String,
    pub metadata: HashMap<String, String>,
    pub repo: String,
    pub avatar_url: String,
    pub github_url: String,
    pub raw_base_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheckResult {
    pub name: String,
    pub repo: String,
    pub has_update: bool,
    pub current_sha: String,
    pub latest_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillsLock {
    version: u32,
    skills: HashMap<String, SkillLockEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillLockEntry {
    source: String,
    sha: String,
    installed_at: DateTime<Utc>,
}

impl Default for SkillsLock {
    fn default() -> Self {
        Self {
            version: 1,
            skills: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct SkillService {
    registry: SkillRegistryClient,
    resolver: SkillResolver,
    storage: StorageService,
    installed_dir: PathBuf,
    list_cache: Arc<moka::future::Cache<String, Vec<Skill>>>,
    browse_cache: Arc<moka::future::Cache<String, RepoBrowseResult>>,
}

impl SkillService {
    pub fn new(
        registry: SkillRegistryClient,
        resolver: SkillResolver,
        storage: StorageService,
        installed_dir: impl Into<PathBuf>,
        cache_config: &CacheConfig,
    ) -> Self {
        let list_cache = Arc::new(
            moka::future::Cache::builder()
                .max_capacity(cache_config.entity_max_capacity)
                .time_to_live(std::time::Duration::from_secs(cache_config.entity_ttl_secs))
                .build(),
        );
        let browse_cache = Arc::new(
            moka::future::Cache::builder()
                .max_capacity(100)
                .time_to_live(std::time::Duration::from_secs(600))
                .build(),
        );

        Self {
            registry,
            resolver,
            storage,
            installed_dir: installed_dir.into(),
            list_cache,
            browse_cache,
        }
    }

    pub fn start_watcher(&self) {
        use notify::{RecursiveMode, Watcher};

        let list_cache = self.list_cache.clone();
        let browse_cache = self.browse_cache.clone();

        let mut dirs_to_watch = vec![self.installed_dir.clone()];
        dirs_to_watch.push(self.resolver.builtin_skills_dir());

        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();

            let mut watcher = match notify::recommended_watcher(tx) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create filesystem watcher for skills, relying on TTL-based cache expiry");
                    return;
                }
            };

            for dir in &dirs_to_watch {
                if dir.exists()
                    && let Err(e) = watcher.watch(dir, RecursiveMode::Recursive)
                {
                    tracing::warn!(dir = %dir.display(), error = %e, "Failed to watch skill directory");
                }
            }

            tracing::info!("Skill filesystem watcher started");

            for event in rx {
                match event {
                    Ok(_) => {
                        list_cache.invalidate_all();
                        browse_cache.invalidate_all();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Skill filesystem watcher error");
                    }
                }
            }
        });
    }

    pub async fn list(
        &self,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        agent_skills: Option<&[String]>,
    ) -> Vec<Skill> {
        let cache_key = format!("{user_handle}/{agent_handle}:{}", skills_hash(agent_skills));
        if let Some(cached) = self.list_cache.get(&cache_key).await {
            return cached;
        }
        let result = self.resolver.list(user_handle, agent_handle, agent_skills);
        self.list_cache.insert(cache_key, result.clone()).await;
        result
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SkillSearchResult>, AppError> {
        let results = self.registry.search(query, 20).await?;
        let lock = self.read_lock();

        Ok(results
            .into_iter()
            .map(|r| {
                let installed = lock
                    .skills
                    .get(&r.name)
                    .is_some_and(|entry| entry.source == r.repo);
                SkillSearchResult {
                    name: r.name,
                    repo: r.repo,
                    avatar_url: r.avatar_url,
                    installs: r.installs,
                    installed,
                }
            })
            .collect())
    }

    pub async fn get_skills(&self, owner_repo: &str) -> Result<RepoBrowseResult, AppError> {
        if let Some(cached) = self.browse_cache.get(owner_repo).await {
            let lock = self.read_lock();
            let skills = cached
                .skills
                .into_iter()
                .map(|mut s| {
                    s.installed = lock
                        .skills
                        .get(&s.name)
                        .is_some_and(|entry| entry.source == owner_repo);
                    s
                })
                .collect();
            return Ok(RepoBrowseResult { skills, ..cached });
        }

        let discovered = self.registry.discover_skills(owner_repo).await?;

        let lock = self.read_lock();
        let skills = discovered
            .into_iter()
            .map(|s| {
                let installed = lock
                    .skills
                    .get(&s.name)
                    .is_some_and(|entry| entry.source == owner_repo);
                RepoBrowseSkill {
                    description: s.description,
                    name: s.name,
                    sha: s.sha,
                    dir_path: s.dir_path,
                    installed,
                }
            })
            .collect();

        let result = RepoBrowseResult {
            repo: owner_repo.to_string(),
            description: String::new(),
            avatar_url: registry::derive_avatar_url(owner_repo),
            skills,
        };

        self.browse_cache
            .insert(owner_repo.to_string(), result.clone())
            .await;

        Ok(result)
    }

    pub async fn preview(&self, repo: &str, skill_name: &str) -> Result<SkillPreview, AppError> {
        let local_content = self.installed_dir.join(skill_name).join("SKILL.md");
        let (content, dir_path) = if local_content.exists() {
            let c = std::fs::read_to_string(&local_content).map_err(|e| {
                AppError::Internal(format!("Failed to read installed SKILL.md: {e}"))
            })?;
            (c, skill_name.to_string())
        } else {
            let browse = self.get_skills(repo).await?;
            let skill = browse
                .skills
                .iter()
                .find(|s| s.name == skill_name)
                .ok_or_else(|| {
                    AppError::NotFound(format!("Skill '{skill_name}' not found in {repo}"))
                })?;
            let c = self
                .registry
                .fetch_skill_content(repo, &skill.dir_path)
                .await?;
            (c, skill.dir_path.clone())
        };

        let owner = repo.split('/').next().unwrap_or(repo);

        let parsed = agent_skills::Skill::parse(&content)
            .map_err(|e| AppError::Validation(format!("Invalid SKILL.md: {e}")))?;

        let mut metadata = HashMap::new();
        if let Some(license) = parsed.frontmatter().license() {
            metadata.insert("license".to_string(), license.to_string());
        }
        if let Some(compat) = parsed.frontmatter().compatibility() {
            metadata.insert("compatibility".to_string(), compat.as_str().to_string());
        }
        if let Some(meta) = parsed.frontmatter().metadata() {
            for (k, v) in meta.iter() {
                metadata.insert(k.to_string(), v.to_string());
            }
        }

        Ok(SkillPreview {
            name: parsed.name().as_str().to_string(),
            description: parsed.description().as_str().to_string(),
            body: parsed.body().to_string(),
            metadata,
            repo: repo.to_string(),
            avatar_url: format!("https://github.com/{owner}.png"),
            github_url: if dir_path.is_empty() {
                format!("https://github.com/{repo}")
            } else {
                format!("https://github.com/{repo}/tree/main/{dir_path}")
            },
            raw_base_url: if dir_path.is_empty() {
                format!("https://raw.githubusercontent.com/{repo}/main")
            } else {
                format!("https://raw.githubusercontent.com/{repo}/main/{dir_path}")
            },
        })
    }

    pub async fn install(
        &self,
        repo: &str,
        skill_name: &str,
        agent: Option<(&crate::core::Handle, &crate::core::Handle)>,
    ) -> Result<SkillListItem, AppError> {
        let (dir_path, sha, description) = self.discover_skill(repo, skill_name).await?;
        let discovered = super::registry::DiscoveredSkill {
            name: skill_name.to_string(),
            description,
            dir_path,
            sha,
        };
        let fetched = self
            .registry
            .fetch_skill_from_cache(repo, &discovered)
            .await?;

        if let Some((user_handle, agent_handle)) = agent {
            let ws = self.storage.agent_workspace(user_handle, agent_handle);
            let skill_base = format!("skills/{}", fetched.name);
            ws.write(&format!("{skill_base}/SKILL.md"), &fetched.content)?;
            for file in &fetched.files {
                ws.write_bytes(&format!("{skill_base}/{}", file.path), &file.content)?;
            }

            self.invalidate_caches().await;

            return Ok(SkillListItem {
                name: fetched.name,
                description: fetched.description,
                source: Some(repo.to_string()),
                installed_at: Some(Utc::now()),
                scope: SkillScope::Agent,
            });
        }

        let skill_dir = self.installed_dir.join(&fetched.name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| AppError::Internal(format!("Failed to create skill directory: {e}")))?;

        std::fs::write(skill_dir.join("SKILL.md"), &fetched.content)
            .map_err(|e| AppError::Internal(format!("Failed to write SKILL.md: {e}")))?;

        for file in &fetched.files {
            let file_path = skill_dir.join(&file.path);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| AppError::Internal(format!("Failed to create directory: {e}")))?;
            }
            std::fs::write(&file_path, &file.content)
                .map_err(|e| AppError::Internal(format!("Failed to write {}: {e}", file.path)))?;
        }

        let now = Utc::now();
        let mut lock = self.read_lock();
        lock.skills.insert(
            fetched.name.clone(),
            SkillLockEntry {
                source: repo.to_string(),
                sha: fetched.sha,
                installed_at: now,
            },
        );
        self.write_lock(&lock)?;

        self.invalidate_caches().await;

        Ok(SkillListItem {
            name: fetched.name,
            description: fetched.description,
            source: Some(repo.to_string()),
            installed_at: Some(now),
            scope: SkillScope::Shared,
        })
    }

    pub async fn install_batch(
        &self,
        repo: &str,
        skill_names: &[String],
        agent: Option<(&crate::core::Handle, &crate::core::Handle)>,
    ) -> Result<Vec<SkillListItem>, AppError> {
        if let Some((user_handle, agent_handle)) = agent {
            return self
                .install_to_agent(user_handle, agent_handle, repo, skill_names)
                .await;
        }
        self.install_to_dir(&self.installed_dir, repo, skill_names, SkillScope::Shared)
            .await
    }

    pub async fn install_batch_for_user(
        &self,
        user_handle: &crate::core::Handle,
        repo: &str,
        skill_names: &[String],
    ) -> Result<Vec<SkillListItem>, AppError> {
        let dir = self.storage.user_skills_path(user_handle);
        self.install_to_dir(&dir, repo, skill_names, SkillScope::User)
            .await
    }

    async fn install_to_agent(
        &self,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        repo: &str,
        skill_names: &[String],
    ) -> Result<Vec<SkillListItem>, AppError> {
        let browse = self.get_skills(repo).await?;
        let now = Utc::now();
        let mut items = Vec::new();
        let ws = self.storage.agent_workspace(user_handle, agent_handle);

        for skill_name in skill_names {
            let skill = browse
                .skills
                .iter()
                .find(|s| s.name == *skill_name)
                .ok_or_else(|| {
                    AppError::NotFound(format!("Skill '{skill_name}' not found in {repo}"))
                })?;
            let discovered = super::registry::DiscoveredSkill {
                name: skill_name.clone(),
                description: skill.description.clone(),
                dir_path: skill.dir_path.clone(),
                sha: skill.sha.clone(),
            };
            let fetched = self
                .registry
                .fetch_skill_from_cache(repo, &discovered)
                .await?;

            let skill_base = format!("skills/{}", fetched.name);
            ws.write(&format!("{skill_base}/SKILL.md"), &fetched.content)?;
            for file in &fetched.files {
                ws.write_bytes(&format!("{skill_base}/{}", file.path), &file.content)?;
            }

            items.push(SkillListItem {
                name: fetched.name,
                description: fetched.description,
                source: Some(repo.to_string()),
                installed_at: Some(now),
                scope: SkillScope::Agent,
            });
        }

        self.invalidate_caches().await;
        Ok(items)
    }

    async fn install_to_dir(
        &self,
        dir: &Path,
        repo: &str,
        skill_names: &[String],
        scope: SkillScope,
    ) -> Result<Vec<SkillListItem>, AppError> {
        let browse = self.get_skills(repo).await?;
        let now = Utc::now();
        let mut items = Vec::new();
        let mut lock = self.read_lock_at(dir);

        for skill_name in skill_names {
            let skill = browse
                .skills
                .iter()
                .find(|s| s.name == *skill_name)
                .ok_or_else(|| {
                    AppError::NotFound(format!("Skill '{skill_name}' not found in {repo}"))
                })?;
            let discovered = super::registry::DiscoveredSkill {
                name: skill_name.clone(),
                description: skill.description.clone(),
                dir_path: skill.dir_path.clone(),
                sha: skill.sha.clone(),
            };
            let fetched = self
                .registry
                .fetch_skill_from_cache(repo, &discovered)
                .await?;

            let skill_dir = dir.join(&fetched.name);
            std::fs::create_dir_all(&skill_dir).map_err(|e| {
                AppError::Internal(format!("Failed to create skill directory: {e}"))
            })?;
            std::fs::write(skill_dir.join("SKILL.md"), &fetched.content)
                .map_err(|e| AppError::Internal(format!("Failed to write SKILL.md: {e}")))?;
            for file in &fetched.files {
                let file_path = skill_dir.join(&file.path);
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        AppError::Internal(format!("Failed to create directory: {e}"))
                    })?;
                }
                std::fs::write(&file_path, &file.content).map_err(|e| {
                    AppError::Internal(format!("Failed to write {}: {e}", file.path))
                })?;
            }
            lock.skills.insert(
                fetched.name.clone(),
                SkillLockEntry {
                    source: repo.to_string(),
                    sha: fetched.sha,
                    installed_at: now,
                },
            );

            items.push(SkillListItem {
                name: fetched.name,
                description: fetched.description,
                source: Some(repo.to_string()),
                installed_at: Some(now),
                scope,
            });
        }

        self.write_lock_at(dir, &lock)?;
        self.invalidate_caches().await;
        Ok(items)
    }

    pub fn list_installed(&self) -> Result<Vec<SkillListItem>, AppError> {
        self.list_installed_in(&self.installed_dir, SkillScope::Shared)
    }

    pub fn list_installed_for_user(
        &self,
        user_handle: &crate::core::Handle,
    ) -> Result<Vec<SkillListItem>, AppError> {
        let dir = self.storage.user_skills_path(user_handle);
        self.list_installed_in(&dir, SkillScope::User)
    }

    pub fn list_builtin(&self) -> Result<Vec<SkillListItem>, AppError> {
        let dir = self.resolver.builtin_skills_dir();
        self.list_installed_in(&dir, SkillScope::Builtin)
    }

    fn list_installed_in(
        &self,
        dir: &Path,
        scope: SkillScope,
    ) -> Result<Vec<SkillListItem>, AppError> {
        let lock = self.read_lock_at(dir);
        let mut items = Vec::new();

        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(items);
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let skill_md = entry.path().join("SKILL.md");

            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                let parsed = parse_frontmatter(&content);
                let description = parsed
                    .metadata
                    .get("description")
                    .cloned()
                    .unwrap_or_default();
                let lock_entry = lock.skills.get(&name);

                items.push(SkillListItem {
                    name,
                    description,
                    source: lock_entry.map(|e| e.source.clone()),
                    installed_at: lock_entry.map(|e| e.installed_at),
                    scope,
                });
            }
        }

        Ok(items)
    }

    pub async fn uninstall_agent_skill(
        &self,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        name: &str,
    ) -> Result<(), AppError> {
        let ws = self.storage.agent_workspace(user_handle, agent_handle);
        let skill_dir = format!("skills/{name}");
        if !ws.exists(&skill_dir) {
            return Err(AppError::NotFound(format!(
                "Skill '{name}' is not installed for this agent"
            )));
        }
        let full_path = ws.base_path().join(&skill_dir);
        std::fs::remove_dir_all(&full_path).map_err(|e| {
            AppError::Internal(format!("Failed to remove agent skill directory: {e}"))
        })?;
        self.invalidate_caches().await;
        Ok(())
    }

    pub async fn uninstall(&self, name: &str) -> Result<(), AppError> {
        self.uninstall_in(&self.installed_dir.clone(), name).await
    }

    pub async fn uninstall_for_user(
        &self,
        user_handle: &crate::core::Handle,
        name: &str,
    ) -> Result<(), AppError> {
        let dir = self.storage.user_skills_path(user_handle);
        self.uninstall_in(&dir, name).await
    }

    async fn uninstall_in(&self, dir: &Path, name: &str) -> Result<(), AppError> {
        let skill_dir = dir.join(name);
        if !skill_dir.exists() {
            return Err(AppError::NotFound(format!(
                "Skill '{name}' is not installed"
            )));
        }

        std::fs::remove_dir_all(&skill_dir)
            .map_err(|e| AppError::Internal(format!("Failed to remove skill directory: {e}")))?;

        let mut lock = self.read_lock_at(dir);
        lock.skills.remove(name);
        self.write_lock_at(dir, &lock)?;

        self.invalidate_caches().await;

        Ok(())
    }

    pub async fn check_updates(&self) -> Result<Vec<UpdateCheckResult>, AppError> {
        let lock = self.read_lock();

        let mut repos: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (name, entry) in &lock.skills {
            repos
                .entry(entry.source.clone())
                .or_default()
                .push((name.clone(), entry.sha.clone()));
        }

        let mut results = Vec::new();

        for (repo, skills) in repos {
            let discovered = match self.get_skills(&repo).await {
                Ok(browse) => browse
                    .skills
                    .iter()
                    .map(|s| (s.name.clone(), s.sha.clone()))
                    .collect::<Vec<_>>(),
                Err(e) => {
                    tracing::warn!(repo = %repo, error = %e, "Failed to check updates for repo");
                    continue;
                }
            };
            for (name, current_sha) in skills {
                let latest_sha = discovered
                    .iter()
                    .find(|(n, _)| n == &name)
                    .map(|(_, sha)| sha.clone())
                    .unwrap_or_default();
                results.push(UpdateCheckResult {
                    has_update: !latest_sha.is_empty() && latest_sha != current_sha,
                    name,
                    repo: repo.clone(),
                    current_sha,
                    latest_sha,
                });
            }
        }

        Ok(results)
    }

    async fn invalidate_caches(&self) {
        self.list_cache.invalidate_all();
        self.browse_cache.invalidate_all();
        self.list_cache.run_pending_tasks().await;
        self.browse_cache.run_pending_tasks().await;
    }

    async fn discover_skill(
        &self,
        repo: &str,
        skill_name: &str,
    ) -> Result<(String, String, String), AppError> {
        let browse = self.get_skills(repo).await?;
        let skill = browse
            .skills
            .iter()
            .find(|s| s.name == skill_name)
            .ok_or_else(|| {
                AppError::NotFound(format!("Skill '{skill_name}' not found in {repo}"))
            })?;
        Ok((
            skill.dir_path.clone(),
            skill.sha.clone(),
            skill.description.clone(),
        ))
    }

    fn read_lock(&self) -> SkillsLock {
        self.read_lock_at(&self.installed_dir)
    }

    fn read_lock_at(&self, dir: &Path) -> SkillsLock {
        let lock_path = dir.join("skills-lock.json");
        std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_lock(&self, lock: &SkillsLock) -> Result<(), AppError> {
        self.write_lock_at(&self.installed_dir.clone(), lock)
    }

    fn write_lock_at(&self, dir: &Path, lock: &SkillsLock) -> Result<(), AppError> {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::Internal(format!("Failed to create skills directory: {e}")))?;

        let lock_path = dir.join("skills-lock.json");
        let json = serde_json::to_string_pretty(lock)
            .map_err(|e| AppError::Internal(format!("Failed to serialize lock file: {e}")))?;

        std::fs::write(&lock_path, json)
            .map_err(|e| AppError::Internal(format!("Failed to write lock file: {e}")))?;

        Ok(())
    }
}

fn skills_hash(skills: Option<&[String]>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    skills.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_file_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageService::new(&crate::core::config::Config::default());
        let resolver = SkillResolver::new("/tmp/test_config", storage.clone(), tmp.path());
        let service = SkillService::new(
            SkillRegistryClient::default(),
            resolver,
            storage,
            tmp.path(),
            &CacheConfig::default(),
        );

        let mut lock = SkillsLock::default();
        lock.skills.insert(
            "test-skill".to_string(),
            SkillLockEntry {
                source: "owner/repo".to_string(),
                sha: "abc123".to_string(),
                installed_at: Utc::now(),
            },
        );

        service.write_lock(&lock).unwrap();
        let read_back = service.read_lock();

        assert_eq!(read_back.version, 1);
        assert!(read_back.skills.contains_key("test-skill"));
        assert_eq!(read_back.skills["test-skill"].source, "owner/repo");
    }

    #[test]
    fn test_list_installed_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageService::new(&crate::core::config::Config::default());
        let resolver = SkillResolver::new("/tmp/test_config", storage.clone(), tmp.path());
        let service = SkillService::new(
            SkillRegistryClient::default(),
            resolver,
            storage,
            tmp.path(),
            &CacheConfig::default(),
        );
        let result = service.list_installed().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_list_installed_with_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\nContent here\n",
        )
        .unwrap();

        let storage = StorageService::new(&crate::core::config::Config::default());
        let resolver = SkillResolver::new("/tmp/test_config", storage.clone(), tmp.path());
        let service = SkillService::new(
            SkillRegistryClient::default(),
            resolver,
            storage,
            tmp.path(),
            &CacheConfig::default(),
        );
        let result = service.list_installed().unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "test-skill");
        assert_eq!(result[0].description, "A test skill");
    }

    #[tokio::test]
    async fn test_uninstall_removes_dir_and_lock_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: Test\n---\n",
        )
        .unwrap();

        let storage = StorageService::new(&crate::core::config::Config::default());
        let resolver = SkillResolver::new("/tmp/test_config", storage.clone(), tmp.path());
        let service = SkillService::new(
            SkillRegistryClient::default(),
            resolver,
            storage,
            tmp.path(),
            &CacheConfig::default(),
        );

        let mut lock = SkillsLock::default();
        lock.skills.insert(
            "test-skill".to_string(),
            SkillLockEntry {
                source: "owner/repo".to_string(),
                sha: "abc".to_string(),
                installed_at: Utc::now(),
            },
        );
        service.write_lock(&lock).unwrap();

        service.uninstall("test-skill").await.unwrap();

        assert!(!skill_dir.exists());
        let lock_after = service.read_lock();
        assert!(!lock_after.skills.contains_key("test-skill"));
    }

    #[tokio::test]
    async fn test_uninstall_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageService::new(&crate::core::config::Config::default());
        let resolver = SkillResolver::new("/tmp/test_config", storage.clone(), tmp.path());
        let service = SkillService::new(
            SkillRegistryClient::default(),
            resolver,
            storage,
            tmp.path(),
            &CacheConfig::default(),
        );
        let result = service.uninstall("nonexistent").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_skills_hash_deterministic() {
        let a = skills_hash(Some(&["foo".to_string(), "bar".to_string()]));
        let b = skills_hash(Some(&["foo".to_string(), "bar".to_string()]));
        assert_eq!(a, b);
    }

    #[test]
    fn test_skills_hash_different_for_different_input() {
        let a = skills_hash(Some(&["foo".to_string()]));
        let b = skills_hash(Some(&["bar".to_string()]));
        assert_ne!(a, b);
    }

    #[test]
    fn test_agent_skills_parse_inline_description() {
        let content = "---\nname: test\ndescription: A simple description.\n---\nBody\n";
        let skill = agent_skills::Skill::parse(content).unwrap();
        assert_eq!(skill.description().as_str(), "A simple description.");
        assert!(skill.body().contains("Body"));
    }

    #[test]
    fn test_agent_skills_parse_folded_scalar_description() {
        let content = "\
---
name: test
description: >
  First paragraph about the skill.

  TRIGGER WHEN:
  - Item one
  - Item two

  SYMPTOMS:
  - Symptom one
  - Symptom two
---
Body content
";
        let skill = agent_skills::Skill::parse(content).unwrap();
        let desc = skill.description().as_str();
        assert!(desc.contains("First paragraph about the skill."));
        assert!(desc.contains("- Item one - Item two"));
    }

    #[test]
    fn test_agent_skills_parse_literal_scalar_description() {
        let content = "\
---
name: test
description: |
  First line.

  TRIGGER WHEN:
  - Item one
  - Item two
---
Body content
";
        let skill = agent_skills::Skill::parse(content).unwrap();
        let desc = skill.description().as_str();
        assert!(desc.contains("- Item one\n- Item two"));
    }
}
