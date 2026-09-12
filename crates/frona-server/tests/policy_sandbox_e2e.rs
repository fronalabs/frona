//! End-to-end tests for the Cedar policy → SandboxPolicy pipeline.
//!
//! Validates the full chain: policies created via PolicyService → evaluate_sandbox_policy →
//! SandboxPolicy with the correct read/write paths, network access, and deny rules.

use std::sync::Arc;

use frona::db::repo::generic::SurrealRepo;
use frona::policy::models::Policy;
use frona::policy::repository::PolicyRepository;
use frona::policy::schema::build_schema;
use frona::policy::service::PolicyService;
use frona::tool::{AgentTool, ToolDefinition, ToolOutput};

use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

struct MockTool {
    name: &'static str,
    defs: Vec<ToolDefinition>,
}

#[async_trait::async_trait]
impl AgentTool for MockTool {
    fn name(&self) -> &str {
        self.name
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.defs.clone()
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: &frona::tool::InferenceContext,
    ) -> Result<ToolOutput, frona::core::error::AppError> {
        Ok(ToolOutput::text("ok"))
    }
}

/// Tests in this file historically used `"user-1"` as both `user_id` and the
/// user-handle literal in Cedar policy text (e.g. `Policy::Agent::"user-1/x"`).
/// Returning a handle equal to the user_id keeps that convention so the
/// existing Cedar literals continue to match.
fn user1_handle() -> &'static frona::core::Handle {
    static H: std::sync::OnceLock<frona::core::Handle> = std::sync::OnceLock::new();
    H.get_or_init(|| frona::core::Handle::try_new("user-1").expect("test handle"))
}

fn agent_ref<'a>(
    agent_handle: &'a frona::core::Handle,
) -> frona::policy::service::SandboxPrincipalRef<'a> {
    frona::policy::service::SandboxPrincipalRef::agent("user-1", user1_handle(), agent_handle)
}

fn mock_def(id: &str, group: &str) -> ToolDefinition {
    ToolDefinition {
        id: id.into(),
        provider_id: group.into(),
        description: id.into(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }
}

async fn setup() -> (Surreal<Db>, PolicyService) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();

    // Seed the user that every test refers to as `user-1` so PolicyService can
    // resolve their username when building `Agent::"username/handle"` UIDs.
    let user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    user_service
        .create(&frona::auth::User {
            id: "user-1".into(),
            handle: frona::handle!("user-1"),
            email: "u1@example.com".into(),
            name: "User One".into(),
            password_hash: String::new(),
            timezone: None,
            groups: Vec::new(),
            deactivated_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .unwrap();

    let schema = build_schema();
    let repo: Arc<dyn PolicyRepository> = Arc::new(SurrealRepo::<Policy>::new(db.clone()));
    let tool_fixture = helpers::app_state::build(&db).await;
    let tool_manager = tool_fixture.state.tool_manager.clone();

    tool_manager
        .register_user_tool(
            "user-1",
            Arc::new(MockTool {
                name: "browser",
                defs: vec![
                    mock_def("browser_navigate", "browser"),
                    mock_def("browser_screenshot", "browser"),
                ],
            }),
        )
        .await;
    tool_manager
        .register_user_tool(
            "user-1",
            Arc::new(MockTool {
                name: "search",
                defs: vec![mock_def("web_search", "search")],
            }),
        )
        .await;

    let storage = frona::storage::StorageService::new(&frona::core::config::Config::default());
    let service = PolicyService::new(repo, schema, tool_manager, storage, user_service);
    service.sync_base_policies().await.unwrap();
    (db, service)
}

// --- Simple scenarios ---

#[tokio::test]
async fn e2e_no_policies_no_sandbox_permissions() {
    let (_db, service) = setup().await;
    let policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::agent(
                "user-1",
                user1_handle(),
                &frona::handle!("agent-a"),
            ),
            false,
        )
        .await
        .unwrap();
    assert!(policy.read_paths.is_empty());
    assert!(policy.write_paths.is_empty());
    assert!(!policy.network_access);
}

#[tokio::test]
async fn e2e_simple_read_permit() {
    let (_db, service) = setup().await;
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-read-data")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/data");"#,
        )
        .await
        .unwrap();

    let policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::agent(
                "user-1",
                user1_handle(),
                &frona::handle!("agent-a"),
            ),
            false,
        )
        .await
        .unwrap();
    assert!(policy.read_paths.contains(&"/data".to_string()));
}

#[tokio::test]
async fn e2e_managed_default_network_grants_access() {
    let (_db, service) = setup().await;
    let managed = cedar_policy::Policy::from_json(
        Some(cedar_policy::PolicyId::new("default-network-access")),
        serde_json::json!({
            "effect": "permit",
            "principal": { "op": "All" },
            "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
            "resource": { "op": "All" },
            "annotations": {
                "config": "sandbox.default_network_access",
                "readonly": "true"
            },
            "conditions": []
        }),
    )
    .unwrap();
    service.register_managed_policy(managed);

    let policy = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("any-agent")), false)
        .await
        .unwrap();
    assert!(policy.network_access);
}

#[tokio::test]
async fn e2e_agent_specific_policy_does_not_affect_other_agents() {
    let (_db, service) = setup().await;
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-only-a")
               permit(principal == Policy::Agent::"user-1/agent-a", action == Policy::Action::"write", resource == Policy::Path::"/output");"#,
        )
        .await
        .unwrap();

    let p_a = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    assert!(p_a.write_paths.contains(&"/output".to_string()));

    let p_b = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-b")), false)
        .await
        .unwrap();
    assert!(p_b.write_paths.is_empty());
}

#[tokio::test]
async fn e2e_forbid_creates_denied_path() {
    let (_db, service) = setup().await;
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-permit-workspace")
               permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace");"#,
        )
        .await
        .unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-forbid-secrets")
               forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/workspace/secrets");"#,
        )
        .await
        .unwrap();

    let policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::agent(
                "user-1",
                user1_handle(),
                &frona::handle!("agent-a"),
            ),
            false,
        )
        .await
        .unwrap();
    assert!(policy.write_paths.contains(&"/workspace".to_string()));
    assert!(
        policy
            .denied_paths
            .contains(&"/workspace/secrets".to_string())
    );
}

// --- Complex scenarios ---

#[tokio::test]
async fn e2e_tool_conditional_policy_applies_when_tool_permitted() {
    let (_db, service) = setup().await;

    // Forbid browser tools for agent-b (overrides base permit)
    service
        .create_policy(
            "user-1",
            "@id(\"agent-b-no-browser\")\nforbid(\n  principal == Policy::Agent::\"user-1/agent-b\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);",
        )
        .await
        .unwrap();

    service
        .create_policy(
            "user-1",
            r#"@id("e2e-browser-data")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/browser-data")
                   when { principal.tools.contains("browser_navigate") };"#,
        )
        .await
        .unwrap();

    let p_with_browser = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    assert!(
        p_with_browser
            .read_paths
            .contains(&"/browser-data".to_string()),
        "agent with browser tools (default) should get /browser-data: {p_with_browser:?}"
    );

    let p_without_browser = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-b")), false)
        .await
        .unwrap();
    assert!(
        !p_without_browser
            .read_paths
            .contains(&"/browser-data".to_string()),
        "agent with browser forbidden should not get /browser-data: {p_without_browser:?}"
    );
}

#[tokio::test]
async fn e2e_forbid_unless_pattern_carves_exceptions() {
    let (_db, service) = setup().await;
    let managed = cedar_policy::Policy::from_json(
        Some(cedar_policy::PolicyId::new("default-network-access")),
        serde_json::json!({
            "effect": "permit",
            "principal": { "op": "All" },
            "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
            "resource": { "op": "All" },
            "annotations": {},
            "conditions": []
        }),
    )
    .unwrap();
    service.register_managed_policy(managed);

    service
        .create_policy(
            "user-1",
            r#"@id("e2e-restrict-x")
               forbid(principal == Policy::Agent::"user-1/agent-x", action == Policy::Action::"connect", resource)
                   unless { resource == Policy::NetworkDestination::"gmail.com" };"#,
        )
        .await
        .unwrap();

    let p_x = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-x")), false)
        .await
        .unwrap();
    assert!(
        p_x.network_destinations.contains(&"gmail.com".to_string()),
        "agent-x should have gmail.com as exception: {p_x:?}"
    );

    let p_y = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-y")), false)
        .await
        .unwrap();
    assert!(
        p_y.network_access,
        "agent-y should still have wildcard network access"
    );
    assert!(
        !p_y.network_destinations.contains(&"gmail.com".to_string()),
        "agent-y should not have gmail.com specifically extracted"
    );
}

#[tokio::test]
async fn e2e_multiple_actions_full_sandbox_policy() {
    let (_db, service) = setup().await;
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-multi-read")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/r");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-multi-write")
               permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/w");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-multi-connect")
               permit(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"api.com");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-multi-bind")
               permit(principal, action == Policy::Action::"bind", resource == Policy::NetworkDestination::"8080");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-multi-block")
               forbid(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"10.0.0.0/8");"#,
        ).await.unwrap();

    let policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::agent(
                "user-1",
                user1_handle(),
                &frona::handle!("agent-a"),
            ),
            false,
        )
        .await
        .unwrap();
    assert!(policy.read_paths.contains(&"/r".to_string()));
    assert!(policy.write_paths.contains(&"/w".to_string()));
    assert!(policy.network_access);
    assert!(policy.network_destinations.contains(&"api.com".to_string()));
    assert!(policy.bind_ports.contains(&8080));
    assert!(policy.blocked_networks.contains(&"10.0.0.0/8".to_string()));
}

#[tokio::test]
async fn e2e_policy_changes_invalidate_sandbox_cache() {
    let (_db, service) = setup().await;
    let p1 = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    assert!(p1.read_paths.is_empty());

    service
        .create_policy(
            "user-1",
            r#"@id("e2e-cache-invalidate")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/added");"#,
        ).await.unwrap();

    let p2 = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    assert!(
        p2.read_paths.contains(&"/added".to_string()),
        "cache should be invalidated after policy creation"
    );
}

#[tokio::test]
async fn e2e_apply_to_sandbox_config_merges_correctly() {
    use frona::tool::sandbox::driver::SandboxConfig;

    let (_db, service) = setup().await;
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-apply-read")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/data");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-apply-write")
               permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/output");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-apply-deny")
               forbid(principal, action == Policy::Action::"write", resource == Policy::Path::"/output/secrets");"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-apply-block")
               forbid(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"10.0.0.0/8");"#,
        ).await.unwrap();

    let policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::agent(
                "user-1",
                user1_handle(),
                &frona::handle!("agent-a"),
            ),
            false,
        )
        .await
        .unwrap();

    let mut config = SandboxConfig {
        workspace_dir: "/workspaces/agent-a".into(),
        additional_read_paths: vec!["/preexisting".into()],
        ..Default::default()
    };
    policy.apply(&mut config);

    assert!(
        config
            .additional_read_paths
            .contains(&"/preexisting".to_string())
    );
    assert!(config.additional_read_paths.contains(&"/data".to_string()));
    assert!(
        config
            .additional_write_paths
            .contains(&"/output".to_string())
    );
    assert!(config.denied_paths.contains(&"/output/secrets".to_string()));
    assert!(config.blocked_networks.contains(&"10.0.0.0/8".to_string()));
}

#[tokio::test]
async fn e2e_complex_real_world_browser_agent() {
    let (_db, service) = setup().await;

    // Default network access for everyone
    let managed = cedar_policy::Policy::from_json(
        Some(cedar_policy::PolicyId::new("default-network-access")),
        serde_json::json!({
            "effect": "permit",
            "principal": { "op": "All" },
            "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
            "resource": { "op": "All" },
            "annotations": {},
            "conditions": []
        }),
    )
    .unwrap();
    service.register_managed_policy(managed);

    // Forbid browser tools for agent-other (agent-web keeps default permit)
    service
        .create_policy(
            "user-1",
            "@id(\"agent-other-no-browser\")\nforbid(\n  principal == Policy::Agent::\"user-1/agent-other\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);",
        )
        .await
        .unwrap();

    // Browser agents get /browser-profiles read+write
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-browser-profiles-read")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/browser-profiles")
                   when { principal.tools.contains("browser_navigate") };"#,
        ).await.unwrap();
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-browser-profiles-write")
               permit(principal, action == Policy::Action::"write", resource == Policy::Path::"/browser-profiles")
                   when { principal.tools.contains("browser_navigate") };"#,
        ).await.unwrap();

    // Block internal network for everyone
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-block-internal")
               forbid(principal, action == Policy::Action::"connect", resource == Policy::NetworkDestination::"10.0.0.0/8");"#,
        ).await.unwrap();

    // Web agent
    let web = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-web")), false)
        .await
        .unwrap();
    assert!(web.read_paths.contains(&"/browser-profiles".to_string()));
    assert!(web.write_paths.contains(&"/browser-profiles".to_string()));
    assert!(web.network_access);
    assert!(web.blocked_networks.contains(&"10.0.0.0/8".to_string()));

    // Non-browser agent
    let other = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-other")), false)
        .await
        .unwrap();
    assert!(!other.read_paths.contains(&"/browser-profiles".to_string()));
    assert!(!other.write_paths.contains(&"/browser-profiles".to_string()));
    assert!(
        other.network_access,
        "all agents have default network access"
    );
    assert!(
        other.blocked_networks.contains(&"10.0.0.0/8".to_string()),
        "internal block applies to all agents"
    );
}

#[tokio::test]
async fn e2e_managed_policy_combined_with_user_policies() {
    let (_db, service) = setup().await;

    // Dynamic: default network
    let managed = cedar_policy::Policy::from_json(
        Some(cedar_policy::PolicyId::new("default-network-access")),
        serde_json::json!({
            "effect": "permit",
            "principal": { "op": "All" },
            "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
            "resource": { "op": "All" },
            "annotations": {},
            "conditions": []
        }),
    )
    .unwrap();
    service.register_managed_policy(managed);

    // User: read /shared
    service
        .create_policy(
            "user-1",
            r#"@id("e2e-combined-read")
               permit(principal, action == Policy::Action::"read", resource == Policy::Path::"/shared");"#,
        ).await.unwrap();

    let policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::agent(
                "user-1",
                user1_handle(),
                &frona::handle!("agent-a"),
            ),
            false,
        )
        .await
        .unwrap();
    assert!(policy.network_access, "managed permit applies");
    assert!(
        policy.read_paths.contains(&"/shared".to_string()),
        "user permit applies"
    );
}

#[tokio::test]
async fn e2e_materialize_creates_policies_and_evaluates_back() {
    let (_db, service) = setup().await;
    // Mirror the managed policy main.rs registers at startup so
    // `network_access: true` rounds-trips through evaluation.
    let managed = cedar_policy::Policy::from_json(
        Some(cedar_policy::PolicyId::new("default-network-access")),
        serde_json::json!({
            "effect": "permit",
            "principal": { "op": "All" },
            "action": { "op": "==", "entity": { "type": "Policy::Action", "id": "connect" } },
            "resource": { "op": "All" },
            "annotations": {},
            "conditions": []
        }),
    )
    .unwrap();
    service.register_managed_policy(managed);

    let entity = frona::policy::reconcile::EntityRef::agent(
        &frona::handle!("user-1"),
        &frona::handle!("agent-a"),
    );
    let input = frona::policy::sandbox::SandboxPolicy {
        read_paths: vec!["/data".into()],
        write_paths: vec!["/work".into()],
        network_access: true,
        ..Default::default()
    };

    service
        .reconcile_sandbox_policy("user-1", entity, &input)
        .await
        .unwrap();

    let evaluated = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    assert!(evaluated.read_paths.contains(&"/data".to_string()));
    assert!(evaluated.write_paths.contains(&"/work".to_string()));
    assert!(evaluated.network_access);
}

#[tokio::test]
async fn e2e_reconcile_no_op_on_equal_input_does_not_touch_rows() {
    let (_db, service) = setup().await;
    let entity = || {
        frona::policy::reconcile::EntityRef::agent(
            &frona::handle!("user-1"),
            &frona::handle!("agent-a"),
        )
    };

    let input = frona::policy::sandbox::SandboxPolicy {
        read_paths: vec!["/data".into()],
        ..Default::default()
    };

    service
        .reconcile_sandbox_policy("user-1", entity(), &input)
        .await
        .unwrap();
    let after_first: Vec<(String, chrono::DateTime<chrono::Utc>)> = service
        .list_policies("user-1")
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.name.starts_with("reconcile-"))
        .map(|p| (p.id, p.updated_at))
        .collect();
    assert!(!after_first.is_empty());

    service
        .reconcile_sandbox_policy("user-1", entity(), &input)
        .await
        .unwrap();
    let after_second: Vec<(String, chrono::DateTime<chrono::Utc>)> = service
        .list_policies("user-1")
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.name.starts_with("reconcile-"))
        .map(|p| (p.id, p.updated_at))
        .collect();

    assert_eq!(
        after_first, after_second,
        "no-op reconcile must not touch any row's updated_at"
    );
}

#[tokio::test]
async fn e2e_reconcile_replaces_stale_policies() {
    let (_db, service) = setup().await;
    let entity = || {
        frona::policy::reconcile::EntityRef::agent(
            &frona::handle!("user-1"),
            &frona::handle!("agent-a"),
        )
    };

    service
        .reconcile_sandbox_policy(
            "user-1",
            entity(),
            &frona::policy::sandbox::SandboxPolicy {
                read_paths: vec!["/foo".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    service
        .reconcile_sandbox_policy(
            "user-1",
            entity(),
            &frona::policy::sandbox::SandboxPolicy {
                read_paths: vec!["/bar".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let evaluated = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    assert!(evaluated.read_paths.contains(&"/bar".to_string()));
    assert!(
        !evaluated.read_paths.contains(&"/foo".to_string()),
        "stale read for /foo should be gone"
    );
}

#[tokio::test]
async fn e2e_reconcile_default_clears_all_for_principal() {
    let (_db, service) = setup().await;
    let entity = || {
        frona::policy::reconcile::EntityRef::agent(
            &frona::handle!("user-1"),
            &frona::handle!("agent-a"),
        )
    };

    let input = frona::policy::sandbox::SandboxPolicy {
        read_paths: vec!["/data".into()],
        write_paths: vec!["/work".into()],
        ..Default::default()
    };
    service
        .reconcile_sandbox_policy("user-1", entity(), &input)
        .await
        .unwrap();
    assert!(
        service
            .list_policies("user-1")
            .await
            .unwrap()
            .iter()
            .any(|p| p.name.starts_with("reconcile-"))
    );

    // `permissive()` (network_access = true, no other rules) emits zero
    // policies — used here to wipe reconciled rows entirely.
    service
        .reconcile_sandbox_policy(
            "user-1",
            entity(),
            &frona::policy::sandbox::SandboxPolicy::permissive(),
        )
        .await
        .unwrap();
    let remaining: Vec<_> = service
        .list_policies("user-1")
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.name.starts_with("reconcile-"))
        .collect();
    assert!(
        remaining.is_empty(),
        "permissive reconcile should wipe all reconciled rules"
    );
}

#[tokio::test]
async fn e2e_reconcile_preserves_user_authored_policies() {
    let (_db, service) = setup().await;
    let entity = || {
        frona::policy::reconcile::EntityRef::agent(
            &frona::handle!("user-1"),
            &frona::handle!("agent-a"),
        )
    };

    // Policy on `delegate_task` action — not in any group that
    // SandboxPolicy reconciliation manages, so it must survive untouched.
    service
        .create_policy(
            "user-1",
            r#"@id("user-hand-delegate")
               permit(principal == Policy::Agent::"user-1/agent-a", action == Policy::Action::"delegate_task", resource);"#,
        )
        .await
        .unwrap();

    let make_input = |paths: Vec<String>| frona::policy::sandbox::SandboxPolicy {
        read_paths: paths,
        ..Default::default()
    };
    service
        .reconcile_sandbox_policy("user-1", entity(), &make_input(vec!["/foo".into()]))
        .await
        .unwrap();
    service
        .reconcile_sandbox_policy("user-1", entity(), &make_input(vec!["/bar".into()]))
        .await
        .unwrap();

    let user_policy = service
        .list_policies("user-1")
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.name == "user-hand-delegate");
    assert!(
        user_policy.is_some(),
        "user-authored policy on unrelated action must survive re-reconcile"
    );
}

#[tokio::test]
async fn e2e_reconcile_for_mcp_principal_isolates_from_agent() {
    let (_db, service) = setup().await;

    let agent_entity = frona::policy::reconcile::EntityRef::agent(
        &frona::handle!("user-1"),
        &frona::handle!("agent-a"),
    );
    let mcp_entity = frona::policy::reconcile::EntityRef::Mcp("user-1/srv-1".into());

    service
        .reconcile_sandbox_policy(
            "user-1",
            agent_entity,
            &frona::policy::sandbox::SandboxPolicy {
                read_paths: vec!["/agent-data".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    service
        .reconcile_sandbox_policy(
            "user-1",
            mcp_entity,
            &frona::policy::sandbox::SandboxPolicy {
                read_paths: vec!["/mcp-data".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let agent_policy = service
        .evaluate_sandbox_policy(agent_ref(&frona::handle!("agent-a")), false)
        .await
        .unwrap();
    let mcp_policy = service
        .evaluate_sandbox_policy(
            frona::policy::service::SandboxPrincipalRef::mcp(
                "user-1",
                user1_handle(),
                &frona::handle!("srv-1"),
            ),
            false,
        )
        .await
        .unwrap();

    assert!(agent_policy.read_paths.contains(&"/agent-data".to_string()));
    assert!(!agent_policy.read_paths.contains(&"/mcp-data".to_string()));
    assert!(mcp_policy.read_paths.contains(&"/mcp-data".to_string()));
    assert!(!mcp_policy.read_paths.contains(&"/agent-data".to_string()));
}

mod helpers;
