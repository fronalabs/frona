use std::collections::HashSet;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityUid, PolicySet, Request,
    RestrictedExpression, Schema,
};
use moka::future::Cache;
use tokio::sync::Mutex as AsyncMutex;

use crate::agent::models::Agent;
use crate::auth::{User, UserService};
use crate::core::error::AppError;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPrincipalKind {
    Agent,
    Mcp,
    App,
}

impl SandboxPrincipalKind {
    fn cache_prefix(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Mcp => "mcp_server",
            Self::App => "app",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SandboxPrincipalRef<'a> {
    pub user_id: &'a str,
    pub user_handle: &'a crate::core::Handle,
    pub handle: &'a crate::core::Handle,
    pub kind: SandboxPrincipalKind,
}

impl<'a> SandboxPrincipalRef<'a> {
    pub fn agent(
        user_id: &'a str,
        user_handle: &'a crate::core::Handle,
        handle: &'a crate::core::Handle,
    ) -> Self {
        Self {
            user_id,
            user_handle,
            handle,
            kind: SandboxPrincipalKind::Agent,
        }
    }

    pub fn mcp(
        user_id: &'a str,
        user_handle: &'a crate::core::Handle,
        handle: &'a crate::core::Handle,
    ) -> Self {
        Self {
            user_id,
            user_handle,
            handle,
            kind: SandboxPrincipalKind::Mcp,
        }
    }

    pub fn app(
        user_id: &'a str,
        user_handle: &'a crate::core::Handle,
        handle: &'a crate::core::Handle,
    ) -> Self {
        Self {
            user_id,
            user_handle,
            handle,
            kind: SandboxPrincipalKind::App,
        }
    }

    pub fn user_id(&self) -> &'a str {
        self.user_id
    }

    fn cache_key(&self) -> String {
        // Prefix with user_id (not handle) so PolicyService::invalidate_cache,
        // which scans `sandbox_cache` for keys starting with `{user_id}:`,
        // actually evicts entries after a reconcile.
        format!(
            "{}:{}:{}",
            self.user_id,
            self.kind.cache_prefix(),
            self.handle
        )
    }
}

use super::models::{AuthorizationDecision, Policy, PolicyAction, PolicyResource};
use super::reconcile::{
    AccessGroup, AccessIntent, AccessOverride, Edit, EntityRef, GroupConflict,
    PolicyReconcileTarget, PolicyReconciliationError, PolicyReconciliationPlan,
    PolicyReconciliationResult, fingerprint, plan_for_group,
};
use super::repository::PolicyRepository;
use super::schema::{
    action_entity_uid, agent_entity_uid, build_agent_entities, build_agent_principal_entity,
    build_app_principal_entity, build_mcp_principal_entity, build_tool_entities,
    extract_annotations, references_agent, tool_entity_uid,
};

const BASE_POLICIES: &str = include_str!("../../../../resources/policy/frona.cedar");

struct CachedPolicySet {
    policy_set: PolicySet,
}

#[derive(Clone)]
pub struct PolicyService {
    repo: Arc<dyn PolicyRepository>,
    schema: Arc<Schema>,
    policy_cache: Cache<String, Arc<CachedPolicySet>>,
    sandbox_cache: Cache<String, Arc<super::sandbox::SandboxPolicy>>,
    decision_cache: Cache<String, AuthorizationDecision>,
    agent_tools_cache: Cache<String, Arc<Vec<String>>>,
    tool_manager: Arc<crate::tool::manager::ToolManager>,
    managed_policies: Arc<RwLock<Vec<cedar_policy::Policy>>>,
    sandbox_disabled: bool,
    storage: crate::storage::StorageService,
    user_service: UserService,
    /// Serializes `commit()`. Dry-run skips this; fingerprint check covers staleness.
    commit_lock: Arc<AsyncMutex<()>>,
}

impl PolicyService {
    pub fn new(
        repo: Arc<dyn PolicyRepository>,
        schema: Arc<Schema>,
        tool_manager: Arc<crate::tool::manager::ToolManager>,
        storage: crate::storage::StorageService,
        user_service: UserService,
    ) -> Self {
        Self::with_sandbox_disabled(repo, schema, tool_manager, storage, user_service, false)
    }

    pub fn with_sandbox_disabled(
        repo: Arc<dyn PolicyRepository>,
        schema: Arc<Schema>,
        tool_manager: Arc<crate::tool::manager::ToolManager>,
        storage: crate::storage::StorageService,
        user_service: UserService,
        sandbox_disabled: bool,
    ) -> Self {
        let policy_cache = Cache::builder().max_capacity(1000).build();
        let sandbox_cache = Cache::builder().max_capacity(1000).build();
        let decision_cache = Cache::builder().max_capacity(10_000).build();
        let agent_tools_cache = Cache::builder().max_capacity(1000).build();

        Self {
            repo,
            schema,
            policy_cache,
            sandbox_cache,
            decision_cache,
            agent_tools_cache,
            tool_manager,
            managed_policies: Arc::new(RwLock::new(Vec::new())),
            sandbox_disabled,
            storage,
            user_service,
            commit_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    pub fn register_managed_policy(&self, policy: cedar_policy::Policy) {
        self.managed_policies.write().unwrap().push(policy);
        self.invalidate_all_caches();
    }

    pub fn managed_policies(&self) -> Vec<cedar_policy::Policy> {
        self.managed_policies.read().unwrap().clone()
    }

    pub async fn create_policy(
        &self,
        user_id: &str,
        policy_text: &str,
    ) -> Result<Policy, AppError> {
        self.validate_policy_text(policy_text)?;

        let (name, description) = extract_annotations(policy_text);
        let name =
            name.ok_or_else(|| AppError::Validation("Policy must have an @id annotation".into()))?;

        if self.repo.find_by_name(user_id, &name).await?.is_some() {
            return Err(AppError::Validation(format!(
                "Policy with @id \"{name}\" already exists"
            )));
        }

        let now = chrono::Utc::now();
        let policy = Policy {
            id: crate::core::repository::new_id(),
            user_id: Some(user_id.to_string()),
            name,
            description: description.unwrap_or_default(),
            policy_text: policy_text.to_string(),
            enabled: true,
            created_at: now,
            updated_at: now,
        };

        let created = self.repo.create(&policy).await?;
        self.invalidate_cache(user_id).await;
        Ok(created)
    }

    pub async fn update_policy(
        &self,
        user_id: &str,
        id: &str,
        policy_text: &str,
    ) -> Result<Policy, AppError> {
        self.validate_policy_text(policy_text)?;

        let mut policy = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound("Policy not found".into()))?;

        if policy.user_id.as_deref() != Some(user_id) {
            return Err(AppError::Forbidden("Not your policy".into()));
        }

        let (name, description) = extract_annotations(policy_text);
        if let Some(ref new_name) = name
            && new_name != &policy.name
            && self.repo.find_by_name(user_id, new_name).await?.is_some()
        {
            return Err(AppError::Validation(format!(
                "Policy with @id \"{new_name}\" already exists"
            )));
        }

        policy.policy_text = policy_text.to_string();
        if let Some(name) = name {
            policy.name = name;
        }
        if let Some(desc) = description {
            policy.description = desc;
        }
        policy.updated_at = chrono::Utc::now();

        let updated = self.repo.update(&policy).await?;
        self.invalidate_cache(user_id).await;
        Ok(updated)
    }

    pub async fn delete_policy(&self, user_id: &str, id: &str) -> Result<(), AppError> {
        let policy = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound("Policy not found".into()))?;

        if policy.user_id.as_deref() != Some(user_id) {
            return Err(AppError::Forbidden("Not your policy".into()));
        }

        self.repo.delete(id).await?;
        self.invalidate_cache(user_id).await;
        Ok(())
    }

    pub async fn delete_policy_by_name(&self, user_id: &str, name: &str) -> Result<(), AppError> {
        let policy = self
            .repo
            .find_by_name(user_id, name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Policy with @id \"{name}\" not found")))?;

        self.repo.delete(&policy.id).await?;
        self.invalidate_cache(user_id).await;
        Ok(())
    }

    pub async fn find_by_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<Policy>, AppError> {
        self.repo.find_by_name(user_id, name).await
    }

    pub async fn list_system_policies(&self) -> Result<Vec<Policy>, AppError> {
        self.repo.find_system_policies().await
    }

    pub async fn list_policies(&self, user_id: &str) -> Result<Vec<Policy>, AppError> {
        self.repo.find_by_user_id(user_id).await
    }

    pub async fn get_policy(&self, user_id: &str, id: &str) -> Result<Policy, AppError> {
        let policy = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| AppError::NotFound("Policy not found".into()))?;

        if policy.user_id.as_deref() != Some(user_id) {
            return Err(AppError::Forbidden("Not your policy".into()));
        }

        Ok(policy)
    }

    pub fn validate_policy_text(&self, policy_text: &str) -> Result<(), AppError> {
        super::validation::validate_syntax(policy_text)
    }

    pub async fn validate_policy_entities(
        &self,
        user_id: &str,
        policy_text: &str,
    ) -> Result<Vec<String>, AppError> {
        let all_groups = self.tool_manager.tool_groups(user_id).await;
        let all_defs = self.tool_manager.definitions(user_id).await;
        super::validation::validate_entities(policy_text, &all_groups, &all_defs)
    }

    pub async fn authorize(
        &self,
        user_id: &str,
        agent: &Agent,
        action: PolicyAction,
    ) -> Result<AuthorizationDecision, AppError> {
        let start = std::time::Instant::now();
        let action_name = action.cedar_action_name();

        if agent.handle == "system"
            && matches!(&action, PolicyAction::InvokeTool { tool_name, .. } if tool_name == "manage_policy")
        {
            crate::core::metrics::record_policy_evaluation(action_name, "allow", start.elapsed());
            return Ok(AuthorizationDecision::allow());
        }

        let cache_key = format!("{user_id}:{}:{:?}", agent.handle, action);
        if let Some(hit) = self.decision_cache.get(&cache_key).await {
            crate::core::metrics::record_policy_evaluation(
                action_name,
                if hit.allowed {
                    "allow_cached"
                } else {
                    "deny_cached"
                },
                start.elapsed(),
            );
            return Ok(hit);
        }

        let cached = self.build_policy_set(user_id).await?;
        let user_handle = self.user_service.handle_of(&agent.user_id).await?;

        let principal = agent_entity_uid(&user_handle, &agent.handle);
        let action_uid = action_entity_uid(action.cedar_action_name());
        let resource = match &action {
            PolicyAction::InvokeTool { tool_name, .. } => tool_entity_uid(tool_name),
            PolicyAction::DelegateTask { target_handle, .. }
            | PolicyAction::SendMessage { target_handle, .. } => {
                agent_entity_uid(&user_handle, target_handle)
            }
            PolicyAction::ReceiveSignal {
                connector_id,
                sender,
                ..
            }
            | PolicyAction::ReceiveMessage {
                connector_id,
                sender,
                ..
            } => super::schema::message_source_entity_uid(connector_id, &sender.address),
            PolicyAction::ListUsers | PolicyAction::ManageUsers { .. } => {
                return Err(AppError::Internal(
                    "authorize() called with a user-level action; use authorize_user() instead"
                        .into(),
                ));
            }
        };

        let context = match &action {
            PolicyAction::ReceiveSignal {
                paired_addresses, ..
            }
            | PolicyAction::ReceiveMessage {
                paired_addresses, ..
            } => build_paired_addresses_context(paired_addresses)?,
            _ => Context::empty(),
        };

        let entities = match &action {
            PolicyAction::InvokeTool {
                tool_name,
                tool_group,
            } => {
                let tool_entities = build_tool_entities(tool_name, tool_group);
                let principal_tools = self
                    .resolve_agent_tools(user_id, &user_handle, &agent.handle, &cached.policy_set)
                    .await?;
                let principal_entity =
                    build_agent_principal_entity(&user_handle, &agent.handle, &principal_tools);
                Entities::from_entities(
                    tool_entities
                        .iter()
                        .cloned()
                        .chain(std::iter::once(principal_entity)),
                    None,
                )
                .unwrap_or(tool_entities)
            }
            PolicyAction::DelegateTask { target_handle, .. }
            | PolicyAction::SendMessage { target_handle, .. } => {
                let all_defs = self.tool_manager.definitions(user_id).await;
                let mut principal_tools = Vec::new();
                let mut target_tools = Vec::new();
                for def in &all_defs {
                    let resource = PolicyResource::Tool {
                        id: def.id.clone(),
                        group: def.provider_id.clone(),
                    };
                    if self.is_permitted(
                        &user_handle,
                        &agent.handle,
                        &resource,
                        &cached.policy_set,
                    )? {
                        principal_tools.push(def.id.clone());
                    }
                    if self.is_permitted(
                        &user_handle,
                        target_handle,
                        &resource,
                        &cached.policy_set,
                    )? {
                        target_tools.push(def.id.clone());
                    }
                }
                build_agent_entities(
                    &user_handle,
                    &agent.handle,
                    &principal_tools,
                    target_handle,
                    &target_tools,
                )
            }
            PolicyAction::ReceiveSignal {
                connector_id,
                channel_handle,
                sender,
                ..
            }
            | PolicyAction::ReceiveMessage {
                connector_id,
                channel_handle,
                sender,
                ..
            } => {
                let all_defs = self.tool_manager.definitions(user_id).await;
                let mut agent_tools = Vec::new();
                for def in &all_defs {
                    let resource = PolicyResource::Tool {
                        id: def.id.clone(),
                        group: def.provider_id.clone(),
                    };
                    if self.is_permitted(
                        &user_handle,
                        &agent.handle,
                        &resource,
                        &cached.policy_set,
                    )? {
                        agent_tools.push(def.id.clone());
                    }
                }
                super::schema::build_message_source_entities(
                    &user_handle,
                    &agent.handle,
                    &agent_tools,
                    connector_id,
                    channel_handle,
                    sender,
                )
            }
            PolicyAction::ListUsers | PolicyAction::ManageUsers { .. } => {
                return Err(AppError::Internal(
                    "authorize() called with a user-level action; use authorize_user() instead"
                        .into(),
                ));
            }
        };

        let decision = self.evaluate_request(
            action_name,
            principal,
            action_uid,
            resource,
            context,
            &cached.policy_set,
            &entities,
            start,
        )?;
        self.decision_cache
            .insert(cache_key, decision.clone())
            .await;
        Ok(decision)
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_request(
        &self,
        action_name: &str,
        principal: cedar_policy::EntityUid,
        action: cedar_policy::EntityUid,
        resource: cedar_policy::EntityUid,
        context: Context,
        policy_set: &PolicySet,
        entities: &cedar_policy::Entities,
        start: std::time::Instant,
    ) -> Result<AuthorizationDecision, AppError> {
        let request = Request::new(principal, action, resource, context, Some(&self.schema))
            .map_err(|e| AppError::Internal(format!("Policy request error: {e}")))?;

        let authorizer = Authorizer::new();
        let response = authorizer.is_authorized(&request, policy_set, entities);

        match response.decision() {
            Decision::Allow => {
                crate::core::metrics::record_policy_evaluation(
                    action_name,
                    "allow",
                    start.elapsed(),
                );
                Ok(AuthorizationDecision::allow())
            }
            Decision::Deny => {
                let reasons: Vec<String> = response
                    .diagnostics()
                    .errors()
                    .map(|e| e.to_string())
                    .collect();
                let diag = if reasons.is_empty() {
                    "No matching permit policy".to_string()
                } else {
                    reasons.join("; ")
                };
                crate::core::metrics::record_policy_evaluation(
                    action_name,
                    "deny",
                    start.elapsed(),
                );
                Ok(AuthorizationDecision::deny(diag))
            }
        }
    }

    /// Uses `User` as principal; target User or `User::"*"` as resource.
    pub async fn authorize_user(
        &self,
        user: &User,
        action: PolicyAction,
    ) -> Result<AuthorizationDecision, AppError> {
        let start = std::time::Instant::now();
        let action_name = action.cedar_action_name();

        let target_id = match &action {
            PolicyAction::ListUsers => "*".to_string(),
            PolicyAction::ManageUsers { target_user_id } => target_user_id.clone(),
            _ => {
                return Err(AppError::Internal(
                    "authorize_user() called with a non-user action; use authorize() instead"
                        .into(),
                ));
            }
        };

        let cached = self.build_policy_set(&user.id).await?;
        let principal = super::schema::user_entity_uid(&user.id);
        let action_uid = action_entity_uid(action.cedar_action_name());
        let resource = super::schema::user_entity_uid(&target_id);
        let entities =
            super::schema::build_user_action_entities(&user.id, &user.groups, &target_id);

        self.evaluate_request(
            action_name,
            principal,
            action_uid,
            resource,
            Context::empty(),
            &cached.policy_set,
            &entities,
            start,
        )
    }

    /// Group-level authorization check: is `agent` allowed the given tool group?
    /// Used to derive ambient grants (e.g. read access to the memory dir when the
    /// agent has the `memory` tool group).
    pub async fn is_tool_group_permitted(
        &self,
        user_id: &str,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        group: &str,
    ) -> Result<bool, AppError> {
        let cached = self.build_policy_set(user_id).await?;
        let resource = PolicyResource::ToolGroup {
            group: group.to_string(),
        };
        self.is_permitted(user_handle, agent_handle, &resource, &cached.policy_set)
    }

    fn is_permitted(
        &self,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        resource: &PolicyResource,
        policy_set: &PolicySet,
    ) -> Result<bool, AppError> {
        let (entities, resource_uid) = match resource {
            PolicyResource::Tool { id, group } => {
                (build_tool_entities(id, group), tool_entity_uid(id))
            }
            PolicyResource::ToolGroup { group } => {
                (build_tool_entities(group, group), tool_entity_uid(group))
            }
        };
        let principal = agent_entity_uid(user_handle, agent_handle);
        let action_uid = action_entity_uid("invoke_tool");
        let context = Context::empty();

        let request = Request::new(
            principal,
            action_uid,
            resource_uid,
            context,
            Some(&self.schema),
        )
        .map_err(|e| AppError::Internal(format!("Policy request error: {e}")))?;

        let authorizer = Authorizer::new();
        let response = authorizer.is_authorized(&request, policy_set, &entities);
        Ok(response.decision() == Decision::Allow)
    }

    /// `resolve_paths=false` returns raw `user://`/`agent://` for UI;
    /// `true` translates to absolute host paths for sandbox drivers.
    pub async fn evaluate_sandbox_policy(
        &self,
        principal: SandboxPrincipalRef<'_>,
        resolve_paths: bool,
    ) -> Result<Arc<super::sandbox::SandboxPolicy>, AppError> {
        if self.sandbox_disabled {
            return Ok(Arc::new(super::sandbox::SandboxPolicy::permissive()));
        }

        let user_id = principal.user_id();
        let key = principal.cache_key();
        let raw = if let Some(cached) = self.sandbox_cache.get(&key).await {
            cached
        } else {
            let cached = self.build_policy_set(user_id).await?;

            let principal_entity = match principal.kind {
                SandboxPrincipalKind::Agent => {
                    let tools = self
                        .resolve_agent_tools(
                            principal.user_id,
                            principal.user_handle,
                            principal.handle,
                            &cached.policy_set,
                        )
                        .await?;
                    build_agent_principal_entity(principal.user_handle, principal.handle, &tools)
                }
                SandboxPrincipalKind::Mcp => {
                    build_mcp_principal_entity(principal.user_handle, principal.handle)
                }
                SandboxPrincipalKind::App => {
                    build_app_principal_entity(principal.user_handle, principal.handle)
                }
            };

            let policy =
                super::sandbox::evaluate_sandbox_policy(&cached.policy_set, principal_entity);
            let arc = Arc::new(policy);
            self.sandbox_cache.insert(key, arc.clone()).await;
            arc
        };

        if !resolve_paths {
            return Ok(raw);
        }
        let mut resolved = (*raw).clone();
        resolved.resolve_virtual_paths(&self.storage, principal.user_handle);
        Ok(Arc::new(resolved))
    }

    async fn resolve_agent_tools(
        &self,
        user_id: &str,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
        policy_set: &PolicySet,
    ) -> Result<Vec<String>, AppError> {
        let cache_key = format!("{user_id}:{agent_handle}");
        if let Some(cached) = self.agent_tools_cache.get(&cache_key).await {
            return Ok((*cached).clone());
        }

        let all_defs = self.tool_manager.definitions(user_id).await;
        let mut tools = Vec::new();
        for def in all_defs {
            let resource = PolicyResource::Tool {
                id: def.id.clone(),
                group: def.provider_id.clone(),
            };
            if self.is_permitted(user_handle, agent_handle, &resource, policy_set)? {
                tools.push(def.id);
            }
        }
        self.agent_tools_cache
            .insert(cache_key, Arc::new(tools.clone()))
            .await;
        Ok(tools)
    }

    /// Per-group planning; unreachable intents surface as conflicts (commit refuses any).
    pub async fn reconcile(
        &self,
        user_id: &str,
        target: PolicyReconcileTarget,
    ) -> Result<PolicyReconciliationPlan, AppError> {
        let user_policies = self.repo.find_by_user_id(user_id).await?;
        let system_policies = self.repo.find_system_policies().await?;
        let live: Vec<&super::models::Policy> =
            user_policies.iter().chain(system_policies.iter()).collect();
        let fingerprint_value = fingerprint(&live);

        let managed = self.managed_policies();

        let mut all_edits: Vec<Edit> = Vec::new();
        let mut all_conflicts: Vec<GroupConflict> = Vec::new();

        let (entities, hierarchy) =
            build_entities_for_target(&target, user_id, &self.tool_manager).await;

        let ctx = super::reconcile::PlanCtx {
            live: &live,
            managed: &managed,
            schema: &self.schema,
            entities: &entities,
            hierarchy: &hierarchy,
        };

        for group in &target.groups {
            match plan_for_group(group, &ctx) {
                Ok(edits) => all_edits.extend(edits),
                Err(conflict) => all_conflicts.push(conflict),
            }
        }

        Ok(PolicyReconciliationPlan {
            user_id: user_id.to_string(),
            fingerprint: fingerprint_value,
            edits: all_edits,
            conflicts: all_conflicts,
        })
    }

    pub async fn commit(
        &self,
        plan: PolicyReconciliationPlan,
        _force: bool,
    ) -> Result<PolicyReconciliationResult, PolicyReconciliationError> {
        if !plan.conflicts.is_empty() {
            return Err(PolicyReconciliationError::Conflicts(plan.conflicts));
        }

        let _guard = self.commit_lock.lock().await;

        let user_policies = self.repo.find_by_user_id(&plan.user_id).await?;
        let system_policies = self.repo.find_system_policies().await?;
        let live: Vec<&super::models::Policy> =
            user_policies.iter().chain(system_policies.iter()).collect();
        let live_fp = fingerprint(&live);
        if live_fp != plan.fingerprint {
            return Err(PolicyReconciliationError::StalePlan);
        }

        let mut deleted_cache: Vec<super::models::Policy> = Vec::new();
        let mut created_ids: Vec<String> = Vec::new();
        let mut result = PolicyReconciliationResult::default();

        for edit in &plan.edits {
            match edit {
                Edit::Delete { policy_id } => {
                    if let Some(p) = self.repo.find_by_id(policy_id).await? {
                        deleted_cache.push(p.clone());
                        self.repo.delete(policy_id).await?;
                        result.deleted += 1;
                    }
                }
                Edit::Create { name, policy_text } => {
                    let now = chrono::Utc::now();
                    let policy = super::models::Policy {
                        id: crate::core::repository::new_id(),
                        user_id: Some(plan.user_id.clone()),
                        name: name.clone(),
                        description: String::new(),
                        policy_text: policy_text.clone(),
                        enabled: true,
                        created_at: now,
                        updated_at: now,
                    };
                    let created = self.repo.create(&policy).await?;
                    created_ids.push(created.id);
                    result.created += 1;
                }
            }
        }

        self.invalidate_cache(&plan.user_id).await;
        Ok(result)
    }

    pub async fn reconcile_or_fail(
        &self,
        user_id: &str,
        target: PolicyReconcileTarget,
    ) -> Result<PolicyReconciliationResult, PolicyReconciliationError> {
        let plan = self.reconcile(user_id, target).await?;
        self.commit(plan, false).await
    }

    pub async fn reconcile_sandbox_policy(
        &self,
        user_id: &str,
        principal: EntityRef,
        sandbox_policy: &super::sandbox::SandboxPolicy,
    ) -> Result<PolicyReconciliationResult, PolicyReconciliationError> {
        let groups = sandbox_policy_to_groups(&principal, sandbox_policy);
        self.reconcile_or_fail(user_id, PolicyReconcileTarget { groups })
            .await
    }

    /// Closed-world: every registered tool not in `selected` is explicitly denied.
    pub async fn reconcile_agent_tools(
        &self,
        user_id: &str,
        agent_id: &str,
        selected: &[String],
    ) -> Result<PolicyReconciliationResult, PolicyReconciliationError> {
        let universe = self.tool_manager.definitions(user_id).await;
        let selected_set: HashSet<&str> = selected.iter().map(String::as_str).collect();
        let principal = EntityRef::Agent(agent_id.to_string());
        let overrides: Vec<AccessOverride> = universe
            .iter()
            .map(|tool| AccessOverride {
                resource: EntityRef::Tool(tool.id.clone()),
                intent: if selected_set.contains(tool.id.as_str()) {
                    AccessIntent::Allow
                } else {
                    AccessIntent::Deny
                },
            })
            .collect();
        let target = PolicyReconcileTarget {
            groups: vec![AccessGroup {
                principal,
                action: "invoke_tool".into(),
                default: None,
                overrides,
            }],
        };
        self.reconcile_or_fail(user_id, target).await
    }

    pub async fn delete_agent_policies(
        &self,
        user_id: &str,
        user_handle: &crate::core::Handle,
        agent_handle: &crate::core::Handle,
    ) -> Result<(), AppError> {
        let all_policies = self.repo.find_by_user_id(user_id).await?;
        let mut ids_to_delete = Vec::new();

        for policy in &all_policies {
            if references_agent(&policy.policy_text, user_handle, agent_handle) {
                ids_to_delete.push(policy.id.clone());
            }
        }

        if !ids_to_delete.is_empty() {
            self.repo.delete_by_ids(&ids_to_delete).await?;
            self.invalidate_cache(user_id).await;
        }

        Ok(())
    }

    pub async fn sync_base_policies(&self) -> Result<(), AppError> {
        let base_policy_set = PolicySet::from_str(BASE_POLICIES)
            .map_err(|e| AppError::Internal(format!("Failed to parse base policies: {e}")))?;

        let existing = self.repo.find_system_policies().await?;
        let existing_ids: Vec<String> = existing.iter().map(|p| p.id.clone()).collect();
        if !existing_ids.is_empty() {
            self.repo.delete_by_ids(&existing_ids).await?;
        }

        for policy in base_policy_set.policies() {
            let Some(id) = policy.annotation("id") else {
                continue;
            };

            let description = policy.annotation("description").unwrap_or("").to_string();
            let policy_text = policy.to_string();
            let now = chrono::Utc::now();

            let record = Policy {
                id: crate::core::repository::new_id(),
                user_id: None,
                name: id.to_string(),
                description,
                policy_text,
                enabled: true,
                created_at: now,
                updated_at: now,
            };

            self.repo.create(&record).await?;
            tracing::info!(policy_id = id, "Synced base policy");
        }

        self.invalidate_all_caches();
        Ok(())
    }

    async fn build_policy_set(&self, user_id: &str) -> Result<Arc<CachedPolicySet>, AppError> {
        let key = user_id.to_string();

        if let Some(cached) = self.policy_cache.get(&key).await {
            return Ok(cached);
        }

        let system_policies = self.repo.find_system_policies().await?;
        let user_policies = self.repo.find_by_user_id(user_id).await?;

        let mut combined = String::new();
        for policy in system_policies.iter().chain(user_policies.iter()) {
            if policy.enabled {
                combined.push_str(&policy.policy_text);
                combined.push('\n');
            }
        }

        let mut policy_set = PolicySet::from_str(&combined)
            .map_err(|e| AppError::Internal(format!("Failed to parse policies: {e}")))?;

        for policy in self.managed_policies.read().unwrap().iter() {
            policy_set
                .add(policy.clone())
                .map_err(|e| AppError::Internal(format!("Failed to add managed policy: {e}")))?;
        }

        let cached = Arc::new(CachedPolicySet { policy_set });
        self.policy_cache.insert(key, cached.clone()).await;
        Ok(cached)
    }

    pub async fn invalidate_cache(&self, user_id: &str) {
        self.policy_cache.invalidate(user_id).await;
        let prefix = format!("{user_id}:");
        for entry in self.sandbox_cache.iter() {
            if entry.0.starts_with(&prefix) {
                self.sandbox_cache.invalidate(&*entry.0).await;
            }
        }
        for entry in self.decision_cache.iter() {
            if entry.0.starts_with(&prefix) {
                self.decision_cache.invalidate(&*entry.0).await;
            }
        }
        for entry in self.agent_tools_cache.iter() {
            if entry.0.starts_with(&prefix) {
                self.agent_tools_cache.invalidate(&*entry.0).await;
            }
        }
    }

    pub fn invalidate_all_caches(&self) {
        self.policy_cache.invalidate_all();
        self.sandbox_cache.invalidate_all();
        self.decision_cache.invalidate_all();
        self.agent_tools_cache.invalidate_all();
    }

    pub async fn decision_cache_entry_count(&self) -> u64 {
        self.decision_cache.run_pending_tasks().await;
        self.decision_cache.entry_count()
    }
}

fn build_paired_addresses_context(addresses: &[String]) -> Result<Context, AppError> {
    let set = RestrictedExpression::new_set(
        addresses
            .iter()
            .cloned()
            .map(RestrictedExpression::new_string),
    );
    Context::from_pairs([("paired_addresses".into(), set)])
        .map_err(|e| AppError::Internal(format!("Policy context error: {e}")))
}

fn sandbox_policy_to_groups(
    principal: &EntityRef,
    sb: &super::sandbox::SandboxPolicy,
) -> Vec<AccessGroup> {
    let mut groups = Vec::new();

    let mut read_overrides: Vec<AccessOverride> = sb
        .read_paths
        .iter()
        .map(|p| AccessOverride {
            resource: EntityRef::Path(p.clone()),
            intent: AccessIntent::Allow,
        })
        .collect();
    for p in &sb.denied_paths {
        read_overrides.push(AccessOverride {
            resource: EntityRef::Path(p.clone()),
            intent: AccessIntent::Deny,
        });
    }
    groups.push(AccessGroup {
        principal: principal.clone(),
        action: "read".into(),
        default: None,
        overrides: read_overrides,
    });

    let mut write_overrides: Vec<AccessOverride> = sb
        .write_paths
        .iter()
        .map(|p| AccessOverride {
            resource: EntityRef::Path(p.clone()),
            intent: AccessIntent::Allow,
        })
        .collect();
    for p in &sb.denied_paths {
        write_overrides.push(AccessOverride {
            resource: EntityRef::Path(p.clone()),
            intent: AccessIntent::Deny,
        });
    }
    groups.push(AccessGroup {
        principal: principal.clone(),
        action: "write".into(),
        default: None,
        overrides: write_overrides,
    });

    // network_access=false              → wildcard forbid
    // network_access=true + destinations → allowlist (forbid + unless-carveout)
    // network_access=true + no destinations → defer to baseline (no rule).
    //   Lets `SandboxPolicy::permissive()` act as a wipe sentinel.
    let restrict_connect = !sb.network_access || !sb.network_destinations.is_empty();
    let connect_default = if restrict_connect {
        Some(AccessIntent::Deny)
    } else {
        None
    };
    let mut connect_overrides: Vec<AccessOverride> = Vec::new();
    if sb.network_access {
        for d in &sb.network_destinations {
            connect_overrides.push(AccessOverride {
                resource: EntityRef::NetworkDestination(d.clone()),
                intent: AccessIntent::Allow,
            });
        }
    }
    for d in &sb.blocked_networks {
        connect_overrides.push(AccessOverride {
            resource: EntityRef::NetworkDestination(d.clone()),
            intent: AccessIntent::Deny,
        });
    }
    groups.push(AccessGroup {
        principal: principal.clone(),
        action: "connect".into(),
        default: connect_default,
        overrides: connect_overrides,
    });

    let bind_overrides: Vec<AccessOverride> = sb
        .bind_ports
        .iter()
        .map(|port| AccessOverride {
            resource: EntityRef::NetworkDestination(port.to_string()),
            intent: AccessIntent::Allow,
        })
        .collect();
    groups.push(AccessGroup {
        principal: principal.clone(),
        action: "bind".into(),
        default: None,
        overrides: bind_overrides,
    });

    groups
}

/// Agent principals carry the `tools` attribute (for `principal.tools.contains`
/// rules); `invoke_tool` targets additionally emit Tool→ToolGroup parents so
/// `resource in ToolGroup::"X"` resolves during reconcile verification.
async fn build_entities_for_target(
    target: &PolicyReconcileTarget,
    user_id: &str,
    tool_manager: &Arc<crate::tool::manager::ToolManager>,
) -> (Entities, super::reconcile::ResourceHierarchy) {
    let mut entities: Vec<Entity> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut hierarchy = super::reconcile::ResourceHierarchy::default();

    for group in &target.groups {
        let key = (
            group.principal.cedar_type().to_string(),
            group.principal.id().to_string(),
        );
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);

        let entity = match &group.principal {
            EntityRef::Agent(id) => {
                let tools = resolve_agent_tools_for_principal(tool_manager, user_id).await;
                super::schema::build_agent_principal_entity_for_id(id, &tools)
            }
            EntityRef::Mcp(id) => super::schema::build_mcp_principal_entity_for_id(id),
            EntityRef::App(id) => super::schema::build_app_principal_entity_for_id(id),
            EntityRef::User(id) => Entity::new_no_attrs(
                EntityUid::from_type_name_and_id(
                    super::schema::entity_type_name("User"),
                    cedar_policy::EntityId::new(id),
                ),
                std::collections::HashSet::new(),
            ),
            _ => continue,
        };
        entities.push(entity);
    }

    if target.groups.iter().any(|g| g.action == "invoke_tool") {
        let mut groups_seen: HashSet<String> = HashSet::new();
        for def in tool_manager.definitions(user_id).await {
            let group_uid = EntityUid::from_type_name_and_id(
                super::schema::entity_type_name("ToolGroup"),
                cedar_policy::EntityId::new(&def.provider_id),
            );
            let tool_uid = EntityUid::from_type_name_and_id(
                super::schema::entity_type_name("Tool"),
                cedar_policy::EntityId::new(&def.id),
            );
            let attrs = [(
                "provider_id".into(),
                cedar_policy::RestrictedExpression::new_string(def.provider_id.clone()),
            )]
            .into_iter()
            .collect();
            if let Ok(tool_entity) = Entity::new(
                tool_uid,
                attrs,
                std::collections::HashSet::from([group_uid.clone()]),
            ) {
                entities.push(tool_entity);
            }
            if groups_seen.insert(def.provider_id.clone()) {
                entities.push(Entity::new_no_attrs(
                    group_uid,
                    std::collections::HashSet::new(),
                ));
            }
            hierarchy.add(
                EntityRef::Tool(def.id.clone()),
                EntityRef::ToolGroup(def.provider_id.clone()),
            );
        }
    }

    let entities = Entities::from_entities(entities, None).unwrap_or_else(|_| Entities::empty());
    (entities, hierarchy)
}

/// Populates the full tool set for `principal.tools` planner checks;
/// agent-specific filtering happens later in reconcile evaluation.
async fn resolve_agent_tools_for_principal(
    tool_manager: &Arc<crate::tool::manager::ToolManager>,
    user_id: &str,
) -> Vec<String> {
    tool_manager
        .definitions(user_id)
        .await
        .into_iter()
        .map(|d| d.id)
        .collect()
}
