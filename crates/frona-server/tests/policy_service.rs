use std::sync::Arc;

use frona::agent::models::Agent;
use frona::db::repo::generic::SurrealRepo;
use frona::policy::models::{Policy, PolicyAction};
use frona::policy::repository::PolicyRepository;
use frona::policy::schema::build_schema;
use frona::policy::schema::extract_annotations;
use frona::policy::service::PolicyService;

use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem};

async fn setup() -> (Surreal<Db>, PolicyService) {
    setup_with_extra_tools(&[]).await
}

async fn setup_with_extra_tools(
    extras: &[(&'static str, &'static str, &'static str)],
) -> (Surreal<Db>, PolicyService) {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    frona::db::init::setup_schema(&db).await.unwrap();

    let schema = build_schema();
    let repo: Arc<dyn PolicyRepository> = Arc::new(SurrealRepo::<Policy>::new(db.clone()));
    let tool_fixture = helpers::app_state::build(&db).await;
    let tool_manager = tool_fixture.state.tool_manager.clone();

    use frona::tool::{AgentTool, ToolDefinition, ToolOutput};
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
    fn mock_def(id: &str, group: &str) -> ToolDefinition {
        ToolDefinition {
            id: id.into(),
            provider_id: group.into(),
            description: id.into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }
    tool_manager
        .register_user_tool(
            "user-1",
            std::sync::Arc::new(MockTool {
                name: "browser",
                defs: vec![mock_def("browser_navigate", "browser")],
            }),
        )
        .await;
    tool_manager
        .register_user_tool(
            "user-1",
            std::sync::Arc::new(MockTool {
                name: "voice",
                defs: vec![
                    mock_def("make_voice_call", "voice"),
                    mock_def("hangup_call", "voice"),
                ],
            }),
        )
        .await;
    tool_manager
        .register_user_tool(
            "user-1",
            std::sync::Arc::new(MockTool {
                name: "search",
                defs: vec![mock_def("web_search", "search")],
            }),
        )
        .await;

    for &(owner, tool_id, group) in extras {
        tool_manager
            .register_user_tool(
                "user-1",
                std::sync::Arc::new(MockTool {
                    name: owner,
                    defs: vec![mock_def(tool_id, group)],
                }),
            )
            .await;
    }

    let storage = frona::storage::StorageService::new(&frona::core::config::Config::default());
    let user_service = frona::auth::UserService::new(
        SurrealRepo::new(db.clone()),
        &frona::core::config::CacheConfig::default(),
    );
    // Seed the user that policies/agents in this file all reference.
    user_service
        .create(&frona::auth::User {
            id: "user-1".into(),
            handle: frona::handle!("user-1"),
            email: "u@example.com".into(),
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
    let service = PolicyService::new(repo, schema, tool_manager, storage, user_service);
    service.sync_base_policies().await.unwrap();
    (db, service)
}

fn test_agent(id: &str) -> Agent {
    Agent {
        id: id.to_string(),
        user_id: "user-1".into(),
        handle: frona::core::Handle::try_new(id).unwrap(),
        name: format!("Agent {id}"),
        description: String::new(),
        model_group: "primary".into(),
        enabled: true,
        skills: None,
        sandbox_limits: None,
        max_concurrent_tasks: None,
        avatar: None,
        identity: Default::default(),
        prompt: None,
        heartbeat_interval: None,
        next_heartbeat_at: None,
        heartbeat_chat_id: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn authorize_allows_by_default_with_base_policies() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    let decision = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::InvokeTool {
                tool_name: "browser_navigate".into(),
                tool_group: "browser".into(),
            },
        )
        .await
        .unwrap();

    assert!(decision.allowed);
}

#[tokio::test]
async fn authorize_allows_with_permit_policy() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    service
        .create_policy(
            "user-1",
            "@id(\"test-permit\")\npermit(principal, action, resource);",
        )
        .await
        .unwrap();

    let decision = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::InvokeTool {
                tool_name: "browser_navigate".into(),
                tool_group: "browser".into(),
            },
        )
        .await
        .unwrap();

    assert!(decision.allowed);
}

#[tokio::test]
async fn forbid_overrides_permit() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    service
        .create_policy(
            "user-1",
            "@id(\"allow-all\")\npermit(principal, action, resource);",
        )
        .await
        .unwrap();

    service
        .create_policy(
            "user-1",
            "@id(\"deny-browser\")\nforbid(\n  principal == Policy::Agent::\"user-1/agent-1\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);",
        )
        .await
        .unwrap();

    let browser = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::InvokeTool {
                tool_name: "browser_navigate".into(),
                tool_group: "browser".into(),
            },
        )
        .await
        .unwrap();
    assert!(browser.is_denied());

    let search = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::InvokeTool {
                tool_name: "web_search".into(),
                tool_group: "search".into(),
            },
        )
        .await
        .unwrap();
    assert!(search.allowed);
}

#[tokio::test]
async fn base_policies_include_delegation_and_communication() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    let delegate = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::DelegateTask {
                target_agent_id: "agent-2".into(),
                target_handle: frona::handle!("agent-2"),
            },
        )
        .await
        .unwrap();
    assert!(delegate.allowed);

    let send = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::SendMessage {
                target_agent_id: "agent-2".into(),
                target_handle: frona::handle!("agent-2"),
            },
        )
        .await
        .unwrap();
    assert!(send.allowed);
}

fn unresolved_sender(address: &str) -> frona::policy::models::PolicyContact {
    frona::policy::models::PolicyContact {
        id: format!("unresolved:{}", address),
        user_id: "user-1".into(),
        name: String::new(),
        address: address.into(),
        addresses: vec![address.into()],
    }
}

fn self_sender(user_id: &str, address: &str) -> frona::policy::models::PolicyContact {
    frona::policy::models::PolicyContact {
        id: user_id.into(),
        user_id: user_id.into(),
        name: "Mina".into(),
        address: address.into(),
        addresses: vec![address.into()],
    }
}

#[tokio::test]
async fn receive_message_default_forbids_non_self_source() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    let decision = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: unresolved_sender("+15551234"),
                paired_addresses: Vec::new(),
            },
        )
        .await
        .unwrap();

    assert!(decision.is_denied());
}

#[tokio::test]
async fn receive_message_forbid_blocks_specific_sender() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    service
        .create_policy(
            "user-1",
            "@id(\"block-spam\")\nforbid(\n  principal,\n  action == Policy::Action::\"receive_message\",\n  resource\n)\nwhen { resource.sender.address == \"22000\" };",
        )
        .await
        .unwrap();

    let allowed_msg = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: self_sender("user-1", "+15551234"),
                paired_addresses: vec!["+15551234".into()],
            },
        )
        .await
        .unwrap();
    assert!(allowed_msg.allowed);

    let blocked_msg = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: unresolved_sender("22000"),
                paired_addresses: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert!(blocked_msg.is_denied());
}

#[tokio::test]
async fn receive_message_per_agent_channel_block() {
    let (_db, service) = setup().await;
    let finance = test_agent("finance");
    let personal = test_agent("personal");

    service
        .create_policy(
            "user-1",
            "@id(\"finance-no-sms\")\nforbid(\n  principal == Policy::Agent::\"user-1/finance\",\n  action == Policy::Action::\"receive_message\",\n  resource in Policy::Channel::\"user-1/sms\"\n);",
        )
        .await
        .unwrap();

    let finance_sms = service
        .authorize(
            "user-1",
            &finance,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: self_sender("user-1", "+15551234"),
                paired_addresses: vec!["+15551234".into()],
            },
        )
        .await
        .unwrap();
    assert!(finance_sms.is_denied());

    let personal_sms = service
        .authorize(
            "user-1",
            &personal,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: self_sender("user-1", "+15551234"),
                paired_addresses: vec!["+15551234".into()],
            },
        )
        .await
        .unwrap();
    assert!(personal_sms.allowed);
}

#[tokio::test]
async fn receive_message_self_source_unaffected_by_external_forbid() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    service
        .create_policy(
            "user-1",
            "@id(\"block-spam\")\nforbid(\n  principal,\n  action == Policy::Action::\"receive_message\",\n  resource\n)\nwhen { resource.sender.address == \"22000\" };",
        )
        .await
        .unwrap();

    let self_decision = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: self_sender("user-1", "+15551234"),
                paired_addresses: vec!["+15551234".into()],
            },
        )
        .await
        .unwrap();
    assert!(self_decision.allowed);

    let blocked = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::ReceiveMessage {
                connector_id: "space-sms-personal".into(),
                channel_handle: frona::handle!("sms"),
                sender: unresolved_sender("22000"),
                paired_addresses: vec!["+15551234".into()],
            },
        )
        .await
        .unwrap();
    assert!(blocked.is_denied());
}

#[tokio::test]
async fn policy_crud_lifecycle() {
    let (_db, service) = setup().await;

    let policy = service
        .create_policy(
            "user-1",
            "@id(\"test\")\n@description(\"A test\")\npermit(principal, action, resource);",
        )
        .await
        .unwrap();
    assert_eq!(policy.name, "test");
    assert_eq!(policy.description, "A test");

    let updated = service
        .update_policy(
            "user-1",
            &policy.id,
            "@id(\"test\")\n@description(\"Updated\")\npermit(principal, action, resource);",
        )
        .await
        .unwrap();
    assert_eq!(updated.description, "Updated");

    let fetched = service.get_policy("user-1", &policy.id).await.unwrap();
    assert_eq!(fetched.description, "Updated");

    service.delete_policy("user-1", &policy.id).await.unwrap();
    assert!(service.get_policy("user-1", &policy.id).await.is_err());
}

#[tokio::test]
async fn duplicate_policy_name_rejected() {
    let (_db, service) = setup().await;

    service
        .create_policy(
            "user-1",
            "@id(\"unique\")\npermit(principal, action, resource);",
        )
        .await
        .unwrap();

    let result = service
        .create_policy(
            "user-1",
            "@id(\"unique\")\nforbid(principal, action, resource);",
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn delete_agent_policies_cleans_up() {
    let (_db, service) = setup().await;

    service
        .create_policy(
            "user-1",
            "@id(\"agent-1-allow\")\npermit(\n  principal == Policy::Agent::\"user-1/agent-1\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);",
        )
        .await
        .unwrap();

    service
        .create_policy(
            "user-1",
            "@id(\"agent-1-custom\")\npermit(\n  principal == Policy::Agent::\"user-1/agent-1\",\n  action,\n  resource\n);",
        )
        .await
        .unwrap();

    service
        .delete_agent_policies(
            "user-1",
            &frona::handle!("user-1"),
            &frona::handle!("agent-1"),
        )
        .await
        .unwrap();

    let policies = service.list_policies("user-1").await.unwrap();
    assert!(policies.is_empty());
}

#[tokio::test]
async fn system_agent_always_allowed_manage_policy() {
    let (_db, service) = setup().await;
    let system_agent = test_agent("system");

    let decision = service
        .authorize(
            "user-1",
            &system_agent,
            PolicyAction::InvokeTool {
                tool_name: "manage_policy".into(),
                tool_group: "policy".into(),
            },
        )
        .await
        .unwrap();

    assert!(decision.allowed);
}

#[tokio::test]
async fn cache_invalidation_reflects_new_policy() {
    let (_db, service) = setup().await;
    let agent = test_agent("agent-1");

    let allowed = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::InvokeTool {
                tool_name: "browser_navigate".into(),
                tool_group: "browser".into(),
            },
        )
        .await
        .unwrap();
    assert!(allowed.allowed);

    service
        .create_policy("user-1", "@id(\"deny-browser\")\nforbid(\n  principal == Policy::Agent::\"user-1/agent-1\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);")
        .await
        .unwrap();

    let denied = service
        .authorize(
            "user-1",
            &agent,
            PolicyAction::InvokeTool {
                tool_name: "browser_navigate".into(),
                tool_group: "browser".into(),
            },
        )
        .await
        .unwrap();
    assert!(denied.is_denied());
}

#[tokio::test]
async fn base_policy_restricts_agent_tools_to_system() {
    let (_db, service) = setup().await;
    let regular_agent = test_agent("agent-1");
    let system_agent = test_agent("system");

    let denied = service
        .authorize(
            "user-1",
            &regular_agent,
            PolicyAction::InvokeTool {
                tool_name: "create_agent".into(),
                tool_group: "agent".into(),
            },
        )
        .await
        .unwrap();
    assert!(denied.is_denied());

    let allowed = service
        .authorize(
            "user-1",
            &system_agent,
            PolicyAction::InvokeTool {
                tool_name: "create_agent".into(),
                tool_group: "agent".into(),
            },
        )
        .await
        .unwrap();
    assert!(allowed.allowed);
}

#[tokio::test]
async fn validate_policy_text_catches_syntax_errors() {
    let (_db, service) = setup().await;

    assert!(
        service
            .validate_policy_text("permit(principal, action, resource);")
            .is_ok()
    );
    assert!(service.validate_policy_text("invalid syntax here").is_err());
}

#[tokio::test]
async fn extract_annotations_from_parsed_policy() {
    let (id, desc) = extract_annotations(
        "@id(\"my-policy\")\n@description(\"Does something\")\npermit(principal, action, resource);",
    );
    assert_eq!(id.as_deref(), Some("my-policy"));
    assert_eq!(desc.as_deref(), Some("Does something"));
}

#[tokio::test]
async fn delegation_allowed_when_principal_has_superset() {
    let (_db, service) = setup().await;
    let agent_a = test_agent("agent-a");

    let decision = service
        .authorize(
            "user-1",
            &agent_a,
            PolicyAction::DelegateTask {
                target_agent_id: "agent-b".into(),
                target_handle: frona::handle!("agent-b"),
            },
        )
        .await
        .unwrap();
    assert!(decision.allowed);
}

#[tokio::test]
async fn delegation_denied_when_target_has_more_tools() {
    let (_db, service) = setup().await;
    let agent_a = test_agent("agent-a");

    service
        .create_policy(
            "user-1",
            "@id(\"a-no-browser\")\nforbid(\n  principal == Policy::Agent::\"user-1/agent-a\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);",
        )
        .await
        .unwrap();

    let decision = service
        .authorize(
            "user-1",
            &agent_a,
            PolicyAction::DelegateTask {
                target_agent_id: "agent-b".into(),
                target_handle: frona::handle!("agent-b"),
            },
        )
        .await
        .unwrap();
    assert!(decision.is_denied());
}

#[tokio::test]
async fn delegation_denied_researcher_to_receptionist() {
    let (_db, service) = setup().await;
    let researcher = test_agent("researcher");

    // Receptionist can only use voice tools
    service
        .create_policy(
            "user-1",
            "@id(\"receptionist-voice-only\")\nforbid(\n  principal == Policy::Agent::\"user-1/receptionist\",\n  action == Policy::Action::\"invoke_tool\",\n  resource\n) unless { resource in Policy::ToolGroup::\"voice\" };",
        )
        .await
        .unwrap();

    // Voice tools restricted to receptionist only
    service
        .create_policy(
            "user-1",
            "@id(\"voice-receptionist-only\")\nforbid(\n  principal,\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"voice\"\n) unless { principal == Policy::Agent::\"user-1/receptionist\" };",
        )
        .await
        .unwrap();

    // Researcher doesn't have voice tools, receptionist does
    // containsAll should fail — researcher can't delegate to receptionist
    let decision = service
        .authorize(
            "user-1",
            &researcher,
            PolicyAction::DelegateTask {
                target_agent_id: "receptionist".into(),
                target_handle: frona::handle!("receptionist"),
            },
        )
        .await
        .unwrap();
    assert!(
        decision.is_denied(),
        "researcher should not be able to delegate to receptionist (missing voice tools)"
    );
}

#[tokio::test]
async fn delegation_allowed_researcher_to_developer() {
    let (_db, service) = setup().await;
    let researcher = test_agent("researcher");

    // Voice tools restricted to receptionist only
    service
        .create_policy(
            "user-1",
            "@id(\"voice-receptionist-only\")\nforbid(\n  principal,\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"voice\"\n) unless { principal == Policy::Agent::\"user-1/receptionist\" };",
        )
        .await
        .unwrap();

    // Both researcher and developer have the same tools (everything except voice)
    // containsAll should pass
    let decision = service
        .authorize(
            "user-1",
            &researcher,
            PolicyAction::DelegateTask {
                target_agent_id: "developer".into(),
                target_handle: frona::handle!("developer"),
            },
        )
        .await
        .unwrap();
    assert!(
        decision.allowed,
        "researcher should be able to delegate to developer (same tool set)"
    );
}

#[tokio::test]
async fn sync_base_policies_no_duplicates_on_restart() {
    let (_db, service) = setup().await;

    // setup() already calls sync_base_policies once
    let first = service.list_system_policies().await.unwrap();
    let first_count = first.len();
    assert!(first_count > 0, "base policies should be seeded");

    // Simulate restarts
    service.sync_base_policies().await.unwrap();
    service.sync_base_policies().await.unwrap();
    service.sync_base_policies().await.unwrap();

    let after = service.list_system_policies().await.unwrap();
    assert_eq!(
        after.len(),
        first_count,
        "base policies should not duplicate on restart"
    );

    let names: Vec<&str> = after.iter().map(|p| p.name.as_str()).collect();
    let mut unique_names = names.clone();
    unique_names.sort();
    unique_names.dedup();
    assert_eq!(
        names.len(),
        unique_names.len(),
        "all policy names should be unique"
    );
}

mod reconcile {
    use frona::policy::reconcile::{
        AccessGroup, AccessIntent, AccessOverride, EntityRef, PolicyReconcileTarget,
        PolicyReconciliationError,
    };
    use frona::policy::sandbox::SandboxPolicy;

    use super::setup;

    fn group(
        principal_id: &str,
        action: &str,
        default: Option<AccessIntent>,
        overrides: Vec<(EntityRef, AccessIntent)>,
    ) -> AccessGroup {
        AccessGroup {
            principal: EntityRef::agent(
                &frona::handle!("user-1"),
                &frona::core::Handle::try_new(principal_id).unwrap(),
            ),
            action: action.into(),
            default,
            overrides: overrides
                .into_iter()
                .map(|(resource, intent)| AccessOverride { resource, intent })
                .collect(),
        }
    }

    fn dir(p: &str) -> EntityRef {
        EntityRef::Path(p.into())
    }

    fn netd(d: &str) -> EntityRef {
        EntityRef::NetworkDestination(d.into())
    }

    /// Register the managed `default-network-access` permit. Carveout
    /// (forbid-unless) patterns only make sense when there's an underlying
    /// wildcard permit to override.
    fn register_default_network_access(service: &frona::policy::service::PolicyService) {
        let policy = cedar_policy::Policy::from_json(
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
        .expect("default-network-access");
        service.register_managed_policy(policy);
    }

    #[tokio::test]
    async fn empty_target_is_noop() {
        let (_db, service) = setup().await;
        let plan = service
            .reconcile("user-1", PolicyReconcileTarget::default())
            .await
            .unwrap();
        assert!(plan.is_noop());
    }

    #[tokio::test]
    async fn allow_override_with_baseline_deny_emits_permit() {
        let (_db, service) = setup().await;
        let target = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                None,
                vec![(dir("/x"), AccessIntent::Allow)],
            )],
        };
        let result = service.reconcile_or_fail("user-1", target).await.unwrap();
        assert_eq!(result.created, 1);
        assert_eq!(result.deleted, 0);
    }

    #[tokio::test]
    async fn re_reconcile_same_target_is_noop() {
        let (_db, service) = setup().await;
        let target = || PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                None,
                vec![(dir("/x"), AccessIntent::Allow)],
            )],
        };
        service.reconcile_or_fail("user-1", target()).await.unwrap();
        let plan = service.reconcile("user-1", target()).await.unwrap();
        assert!(plan.is_noop(), "re-run is noop, got plan={plan:?}");
    }

    #[tokio::test]
    async fn dropped_override_deletes_owned_row() {
        let (_db, service) = setup().await;
        let v1 = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                None,
                vec![
                    (dir("/x"), AccessIntent::Allow),
                    (dir("/y"), AccessIntent::Allow),
                ],
            )],
        };
        service.reconcile_or_fail("user-1", v1).await.unwrap();

        let v2 = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                None,
                vec![(dir("/x"), AccessIntent::Allow)],
            )],
        };
        let result = service.reconcile_or_fail("user-1", v2).await.unwrap();
        assert_eq!(result.deleted, 1, "drops the /y permit");
    }

    #[tokio::test]
    async fn default_deny_no_overrides_emits_wildcard_forbid() {
        let (_db, service) = setup().await;
        let target = PolicyReconcileTarget {
            groups: vec![group("aa", "connect", Some(AccessIntent::Deny), vec![])],
        };
        let result = service.reconcile_or_fail("user-1", target).await.unwrap();
        assert_eq!(result.created, 1);
    }

    #[tokio::test]
    async fn default_deny_with_allow_override_emits_carveout() {
        let (_db, service) = setup().await;
        register_default_network_access(&service);
        let target = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "connect",
                Some(AccessIntent::Deny),
                vec![(netd("gmail.com"), AccessIntent::Allow)],
            )],
        };
        let result = service.reconcile_or_fail("user-1", target).await.unwrap();
        assert_eq!(result.created, 1);

        let plan = service
            .reconcile(
                "user-1",
                PolicyReconcileTarget {
                    groups: vec![group(
                        "aa",
                        "connect",
                        Some(AccessIntent::Deny),
                        vec![(netd("gmail.com"), AccessIntent::Allow)],
                    )],
                },
            )
            .await
            .unwrap();
        assert!(plan.is_noop(), "carveout should be kept, got plan={plan:?}");
    }

    #[tokio::test]
    async fn switching_carveout_resources_replaces_in_place() {
        let (_db, service) = setup().await;
        register_default_network_access(&service);
        let v1 = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "connect",
                Some(AccessIntent::Deny),
                vec![
                    (netd("gmail.com"), AccessIntent::Allow),
                    (netd("api.github.com"), AccessIntent::Allow),
                ],
            )],
        };
        service.reconcile_or_fail("user-1", v1).await.unwrap();

        let v2 = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "connect",
                Some(AccessIntent::Deny),
                vec![
                    (netd("gmail.com"), AccessIntent::Allow),
                    (netd("zoom.us"), AccessIntent::Allow),
                ],
            )],
        };
        let result = service.reconcile_or_fail("user-1", v2).await.unwrap();
        assert_eq!(result.deleted, 1, "old carveout deleted");
        assert_eq!(result.created, 1, "new carveout created");
    }

    #[tokio::test]
    async fn user_when_conditional_blocks_allow_returns_conflict() {
        let (_db, service) = setup().await;

        // User-authored complex forbid with `when` clause — not owned by reconcile.
        service
            .create_policy(
                "user-1",
                "@id(\"u-when\")\nforbid(principal == Policy::Agent::\"user-1/aa\", action == Policy::Action::\"read\", resource == Policy::Path::\"/x\")\nwhen { principal.enabled };",
            )
            .await
            .unwrap();

        let target = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                None,
                vec![(dir("/x"), AccessIntent::Allow)],
            )],
        };
        let plan = service.reconcile("user-1", target).await.unwrap();
        assert!(!plan.is_clean(), "should have conflicts");

        let r = service.commit(plan, false).await;
        match r {
            Err(PolicyReconciliationError::Conflicts(c)) => {
                assert!(!c.is_empty());
            }
            _ => panic!("expected Conflicts, got {r:?}"),
        }
    }

    #[tokio::test]
    async fn stale_plan_returns_stale_plan_error() {
        let (_db, service) = setup().await;
        let target = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                None,
                vec![(dir("/x"), AccessIntent::Allow)],
            )],
        };
        let plan = service.reconcile("user-1", target).await.unwrap();

        service
            .create_policy(
                "user-1",
                "@id(\"u-other\")\npermit(principal == Policy::Agent::\"user-1/other\", action == Policy::Action::\"read\", resource == Policy::Path::\"/z\");",
            )
            .await
            .unwrap();

        let r = service.commit(plan, false).await;
        match r {
            Err(PolicyReconciliationError::StalePlan) => {}
            _ => panic!("expected StalePlan, got {r:?}"),
        }
    }

    #[tokio::test]
    async fn sandbox_policy_translation_round_trip() {
        let (_db, service) = setup().await;
        // Allowlist semantics require the managed default-network-access permit
        // (matches production where the network_access feature registers it).
        register_default_network_access(&service);
        let sb = SandboxPolicy {
            read_paths: vec!["/r".into()],
            write_paths: vec!["/w".into()],
            network_access: true,
            network_destinations: vec!["api.example.com".into()],
            bind_ports: vec![8080],
            denied_paths: vec!["/secret".into()],
            blocked_networks: vec!["10.0.0.0/8".into()],
        };

        let result = service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &sb,
            )
            .await
            .expect("commit succeeds");
        assert!(result.created > 0);

        let result2 = service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &sb,
            )
            .await
            .unwrap();
        assert_eq!(result2.created, 0);
        assert_eq!(result2.deleted, 0);
    }

    #[tokio::test]
    async fn sandbox_policy_network_off_emits_wildcard_forbid() {
        let (_db, service) = setup().await;
        let sb = SandboxPolicy {
            network_access: false,
            ..Default::default()
        };
        let result = service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &sb,
            )
            .await
            .unwrap();
        assert_eq!(result.created, 1, "one wildcard forbid for connect");
    }

    #[tokio::test]
    /// With ancestor collapse: providers whose tools are all denied collapse
    /// into a single `forbid(... resource in ToolGroup::"X")` rule rather
    /// than one rule per tool.
    async fn sync_agent_tools_emits_forbid_for_unselected() {
        let (_db, service) = setup().await;
        let result = service
            .reconcile_agent_tools("user-1", "agent-1", &["web_search".into()])
            .await
            .unwrap();
        let policies = service.list_policies("user-1").await.unwrap();
        assert_eq!(result.created, policies.len());
        // Other production tool groups remain unselected too.
        for group in ["browser", "voice"] {
            let rows: Vec<_> = policies
                .iter()
                .filter(|p| p.policy_text.contains(&format!("ToolGroup::\"{group}\"")))
                .collect();
            assert_eq!(rows.len(), 1, "one collapsed forbid for {group}");
            assert!(rows[0].policy_text.contains("forbid"));
        }

        let all: Vec<String> = vec![
            "browser_navigate".into(),
            "make_voice_call".into(),
            "hangup_call".into(),
            "web_search".into(),
        ];
        let result2 = service
            .reconcile_agent_tools("user-1", "agent-1", &all)
            .await
            .unwrap();
        assert_eq!(result2.deleted, 2, "the two ToolGroup forbids get cleared");
        assert_eq!(result2.created, 0);
    }

    /// Base policy `permit invoke_tool unless resource in ToolGroup::"agent"`
    /// must resolve correctly during reconcile: an unselected agent-group tool
    /// is already baseline-deny (no forbid needed), and selecting it requires
    /// an explicit permit.
    #[tokio::test]
    async fn sync_agent_tools_respects_base_tool_group_restrictions() {
        let (_db, service) =
            super::setup_with_extra_tools(&[("agent_mgmt", "manage_agent", "agent")]).await;

        let r1 = service
            .reconcile_agent_tools("user-1", "agent-x", &[])
            .await
            .unwrap();
        let policies = service.list_policies("user-1").await.unwrap();
        assert_eq!(r1.created, policies.len());
        assert!(
            !policies
                .iter()
                .any(|p| p.policy_text.contains("ToolGroup::\"agent\""))
        );
        for group in ["browser", "voice", "search"] {
            assert!(policies.iter().any(|p| {
                p.policy_text.contains(&format!("ToolGroup::\"{group}\""))
                    && p.policy_text.contains("forbid")
            }));
        }

        let r2 = service
            .reconcile_agent_tools("user-1", "agent-x", &["manage_agent".into()])
            .await
            .unwrap();
        // Other groups stay denied. Production has other agent-group tools,
        // so selecting only manage_agent must emit a per-tool permit.
        assert_eq!(
            r2.created, 1,
            "selecting one agent-group tool emits one per-tool permit"
        );
        assert_eq!(r2.deleted, 0);

        let policies = service.list_policies("user-1").await.unwrap();
        let agent_permit = policies
            .iter()
            .find(|p| {
                p.policy_text.contains("Tool::\"manage_agent\"") && p.policy_text.contains("permit")
            })
            .expect("permit row for Tool::manage_agent");
        assert!(!agent_permit.policy_text.contains("ToolGroup::\"agent\""));
    }

    /// Provider with two tools, both denied → one rule on the ToolGroup
    /// instead of two per-tool rules. The 'voice' provider in setup() has
    /// make_voice_call + hangup_call.
    #[tokio::test]
    async fn collapse_full_provider_emits_single_toolgroup_rule() {
        let (_db, service) = setup().await;
        // Allow everything except the entire voice provider.
        service
            .reconcile_agent_tools(
                "user-1",
                "agent-z",
                &["browser_navigate".into(), "web_search".into()],
            )
            .await
            .unwrap();
        let policies = service.list_policies("user-1").await.unwrap();
        let voice_rows: Vec<_> = policies
            .iter()
            .filter(|p| p.policy_text.contains("voice"))
            .collect();
        assert_eq!(
            voice_rows.len(),
            1,
            "one collapsed rule for voice, not per-tool"
        );
        assert!(voice_rows[0].policy_text.contains("resource in"));
        assert!(voice_rows[0].policy_text.contains("ToolGroup::\"voice\""));
    }

    /// When a provider's tools have mixed intents, the planner falls back to
    /// per-tool rules — no ancestor collapse.
    #[tokio::test]
    async fn collapse_partial_provider_falls_back_to_per_tool() {
        let (_db, service) = setup().await;
        // Voice provider: make_voice_call selected, hangup_call not.
        service
            .reconcile_agent_tools("user-1", "agent-m", &["make_voice_call".into()])
            .await
            .unwrap();
        let policies = service.list_policies("user-1").await.unwrap();
        let voice_rows: Vec<_> = policies
            .iter()
            .filter(|p| p.policy_text.contains("voice") || p.policy_text.contains("hangup_call"))
            .collect();
        // Expect a per-tool forbid for hangup_call (mixed → no collapse for voice group).
        assert!(
            voice_rows
                .iter()
                .any(|p| p.policy_text.contains("Tool::\"hangup_call\"")
                    && p.policy_text.contains("forbid")),
            "expected per-tool forbid for hangup_call when voice has mixed intent: {voice_rows:?}"
        );
        assert!(
            !voice_rows
                .iter()
                .any(|p| p.policy_text.contains("ToolGroup::\"voice\"")),
            "should not have collapsed to ToolGroup::voice"
        );
    }

    /// Incremental scenario: state has per-tool forbids, then user adds the
    /// last tool. Diff: existing per-tool rows replaced by one ToolGroup rule.
    #[tokio::test]
    async fn incremental_complete_provider_replaces_per_tool_with_group() {
        let (_db, service) = setup().await;
        // mixed intent, falls back to per-tool forbid for hangup_call.
        let r1 = service
            .reconcile_agent_tools(
                "user-1",
                "agent-i",
                &[
                    "browser_navigate".into(),
                    "web_search".into(),
                    "make_voice_call".into(),
                ],
            )
            .await
            .unwrap();
        let policies = service.list_policies("user-1").await.unwrap();
        assert_eq!(r1.created, policies.len());
        let voice_rows: Vec<_> = policies
            .iter()
            .filter(|p| p.policy_text.contains("voice") || p.policy_text.contains("hangup_call"))
            .collect();
        assert_eq!(voice_rows.len(), 1);
        assert!(voice_rows[0].policy_text.contains("Tool::\"hangup_call\""));
        assert!(voice_rows[0].policy_text.contains("forbid"));

        // collapse to ToolGroup::voice rule. Diff: 1 delete (hangup_call
        // per-tool) + 1 create (ToolGroup::voice).
        let r2 = service
            .reconcile_agent_tools(
                "user-1",
                "agent-i",
                &["browser_navigate".into(), "web_search".into()],
            )
            .await
            .unwrap();
        assert_eq!(r2.deleted, 1, "old per-tool hangup_call forbid deleted");
        assert_eq!(r2.created, 1, "new ToolGroup::voice forbid created");

        let policies = service.list_policies("user-1").await.unwrap();
        let voice_rows: Vec<_> = policies
            .iter()
            .filter(|p| p.policy_text.contains("voice") || p.policy_text.contains("hangup_call"))
            .collect();
        assert_eq!(voice_rows.len(), 1);
        assert!(voice_rows[0].policy_text.contains("resource in"));
        assert!(voice_rows[0].policy_text.contains("ToolGroup::\"voice\""));
    }

    /// Re-running with the same selection produces no DB churn — the existing
    /// ToolGroup rule is recognized as canonical and kept.
    #[tokio::test]
    async fn collapse_re_reconcile_is_noop() {
        let (_db, service) = setup().await;
        let selected = vec!["browser_navigate".into(), "web_search".into()];
        let r1 = service
            .reconcile_agent_tools("user-1", "agent-r", &selected)
            .await
            .unwrap();
        let policies = service.list_policies("user-1").await.unwrap();
        assert_eq!(r1.created, policies.len());
        let voice_rows: Vec<_> = policies
            .iter()
            .filter(|p| p.policy_text.contains("voice"))
            .collect();
        assert_eq!(voice_rows.len(), 1);
        assert!(voice_rows[0].policy_text.contains("ToolGroup::\"voice\""));
        assert!(voice_rows[0].policy_text.contains("forbid"));

        let r2 = service
            .reconcile_agent_tools("user-1", "agent-r", &selected)
            .await
            .unwrap();
        assert_eq!(r2.created, 0, "no churn on re-reconcile");
        assert_eq!(r2.deleted, 0);
    }

    /// Switching from a ToolGroup rule to mixed intents replaces the group
    /// rule with per-tool rules.
    #[tokio::test]
    async fn switching_from_toolgroup_to_per_tool_replaces_correctly() {
        let (_db, service) = setup().await;
        service
            .reconcile_agent_tools(
                "user-1",
                "agent-s",
                &["browser_navigate".into(), "web_search".into()],
            )
            .await
            .unwrap();

        // collapse breaks; emit per-tool forbid for hangup_call.
        let r = service
            .reconcile_agent_tools(
                "user-1",
                "agent-s",
                &[
                    "browser_navigate".into(),
                    "web_search".into(),
                    "make_voice_call".into(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(r.deleted, 1, "ToolGroup::voice forbid deleted");
        assert_eq!(r.created, 1, "per-tool hangup_call forbid emitted");

        let policies = service.list_policies("user-1").await.unwrap();
        assert!(
            !policies
                .iter()
                .any(|p| p.policy_text.contains("ToolGroup::\"voice\"")),
            "ToolGroup::voice rule should be gone"
        );
        assert!(
            policies
                .iter()
                .any(|p| p.policy_text.contains("Tool::\"hangup_call\"")
                    && p.policy_text.contains("forbid"))
        );
    }

    #[tokio::test]
    async fn default_allow_with_deny_overrides_emits_permit_unless_carveout() {
        let (_db, service) = setup().await;
        let target = PolicyReconcileTarget {
            groups: vec![group(
                "aa",
                "read",
                Some(AccessIntent::Allow),
                vec![(dir("/secret"), AccessIntent::Deny)],
            )],
        };
        let result = service.reconcile_or_fail("user-1", target).await.unwrap();
        assert_eq!(result.created, 1);

        let policies = service.list_policies("user-1").await.unwrap();
        let emitted = policies
            .iter()
            .find(|p| p.name.starts_with("reconcile-"))
            .expect("emitted");
        assert!(emitted.policy_text.contains("permit"));
        assert!(emitted.policy_text.contains("unless"));
        assert!(emitted.policy_text.contains("/secret"));
    }

    #[tokio::test]
    async fn delete_flow_wipes_all_reconciled_rows_for_principal() {
        let (_db, service) = setup().await;
        let sb = SandboxPolicy {
            read_paths: vec!["/data".into(), "/logs".into()],
            write_paths: vec!["/output".into()],
            denied_paths: vec!["/secrets".into()],
            bind_ports: vec![8080],
            ..Default::default()
        };
        service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &sb,
            )
            .await
            .unwrap();
        let before = service.list_policies("user-1").await.unwrap();
        let owned_before: Vec<_> = before
            .iter()
            .filter(|p| p.name.starts_with("reconcile-"))
            .collect();
        assert!(
            owned_before.len() >= 4,
            "should have multiple reconciled rows, got {}",
            owned_before.len()
        );

        service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &SandboxPolicy::permissive(),
            )
            .await
            .unwrap();
        let after = service.list_policies("user-1").await.unwrap();
        let owned_after: Vec<_> = after
            .iter()
            .filter(|p| p.name.starts_with("reconcile-"))
            .collect();
        assert!(
            owned_after.is_empty(),
            "all reconciled rows for agent should be gone, got {owned_after:?}"
        );
    }

    #[tokio::test]
    async fn delete_flow_does_not_touch_other_principals_rows() {
        let (_db, service) = setup().await;
        let sb = || SandboxPolicy {
            read_paths: vec!["/data".into()],
            ..Default::default()
        };
        service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &sb(),
            )
            .await
            .unwrap();
        service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("bb")),
                &sb(),
            )
            .await
            .unwrap();

        service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &SandboxPolicy::permissive(),
            )
            .await
            .unwrap();

        let policies = service.list_policies("user-1").await.unwrap();
        let agent_a = policies
            .iter()
            .filter(|p| p.policy_text.contains("Policy::Agent::\"user-1/aa\""))
            .count();
        let agent_b = policies
            .iter()
            .filter(|p| p.policy_text.contains("Policy::Agent::\"user-1/bb\""))
            .count();
        assert_eq!(agent_a, 0, "agent a's rows should be wiped");
        assert!(agent_b > 0, "agent b's rows should be untouched");
    }

    #[tokio::test]
    async fn lifecycle_create_modify_revert_delete() {
        let (_db, service) = setup().await;
        let entity = || EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa"));

        // 1. Create with /data read.
        let v1 = SandboxPolicy {
            read_paths: vec!["/data".into()],
            ..Default::default()
        };
        let r1 = service
            .reconcile_sandbox_policy("user-1", entity(), &v1)
            .await
            .unwrap();
        assert!(r1.created > 0);

        // 2. Modify: add /logs.
        let v2 = SandboxPolicy {
            read_paths: vec!["/data".into(), "/logs".into()],
            ..Default::default()
        };
        let r2 = service
            .reconcile_sandbox_policy("user-1", entity(), &v2)
            .await
            .unwrap();
        assert_eq!(r2.created, 1, "added /logs");
        assert_eq!(r2.deleted, 0);

        // 3. Revert to /data only.
        let r3 = service
            .reconcile_sandbox_policy("user-1", entity(), &v1)
            .await
            .unwrap();
        assert_eq!(r3.deleted, 1, "removed /logs");
        assert_eq!(r3.created, 0);

        // 4. Delete: empty SandboxPolicy.
        let r4 = service
            .reconcile_sandbox_policy("user-1", entity(), &SandboxPolicy::default())
            .await
            .unwrap();
        assert_eq!(r4.deleted, 1, "removed /data");
    }

    #[tokio::test]
    async fn read_group_change_does_not_affect_write_group() {
        let (_db, service) = setup().await;
        let entity = || EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa"));

        // Initial: both read and write to /shared.
        let v1 = SandboxPolicy {
            read_paths: vec!["/shared".into()],
            write_paths: vec!["/shared".into()],
            ..Default::default()
        };
        service
            .reconcile_sandbox_policy("user-1", entity(), &v1)
            .await
            .unwrap();

        let v2 = SandboxPolicy {
            read_paths: vec![],
            write_paths: vec!["/shared".into()],
            ..Default::default()
        };
        let r = service
            .reconcile_sandbox_policy("user-1", entity(), &v2)
            .await
            .unwrap();
        assert_eq!(r.deleted, 1, "read row removed");
        assert_eq!(r.created, 0, "write row untouched");
    }

    #[tokio::test]
    async fn connect_group_handles_mixed_allow_and_deny_overrides() {
        let (_db, service) = setup().await;
        register_default_network_access(&service);
        let entity = || EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa"));
        let sb = SandboxPolicy {
            network_access: true,
            network_destinations: vec!["api.gmail.com".into(), "api.github.com".into()],
            blocked_networks: vec!["10.0.0.0/8".into()],
            ..Default::default()
        };
        service
            .reconcile_sandbox_policy("user-1", entity(), &sb)
            .await
            .unwrap();

        let policies = service.list_policies("user-1").await.unwrap();
        let connect_rows: Vec<_> = policies
            .iter()
            .filter(|p| p.name.starts_with("reconcile-Agent-") && p.policy_text.contains("connect"))
            .collect();
        // network_access: true + destinations → allowlist carveout (forbid all
        // unless one of the listed destinations). The blocked_networks entry
        // matches the default (Deny) and is dropped as redundant.
        assert_eq!(connect_rows.len(), 1);
        assert!(connect_rows[0].policy_text.contains("forbid"));
        assert!(connect_rows[0].policy_text.contains("unless"));
        assert!(connect_rows[0].policy_text.contains("api.gmail.com"));
        assert!(connect_rows[0].policy_text.contains("api.github.com"));
    }

    #[tokio::test]
    async fn user_authored_simple_permit_with_same_shape_is_absorbed() {
        // The planner's ownership check is purely structural (Eq P, Eq A, Eq R,
        // no conditions). A user-authored simple-Eq permit matching a managed
        // shape is treated as owned and replaced/deleted on reconcile. This
        // test locks down that behavior so it's not surprising.
        let (_db, service) = setup().await;
        service
            .create_policy(
                "user-1",
                "@id(\"u-y\")\npermit(principal == Policy::Agent::\"user-1/aa\", action == Policy::Action::\"read\", resource == Policy::Path::\"/y\");",
            )
            .await
            .unwrap();

        // Reconcile with read_paths excluding /y → planner sees the user's
        // permit as owned and deletes it.
        let sb = SandboxPolicy {
            read_paths: vec!["/x".into()],
            ..Default::default()
        };
        service
            .reconcile_sandbox_policy(
                "user-1",
                EntityRef::agent(&frona::handle!("user-1"), &frona::handle!("aa")),
                &sb,
            )
            .await
            .unwrap();
        let after = service.list_policies("user-1").await.unwrap();
        assert!(
            !after.iter().any(|p| p.name == "u-y"),
            "user permit absorbed"
        );
    }
}

// ─── User-level authorization (admin endpoints) ─────────────────────

use frona::auth::User as UserModel;

fn test_user(id: &str, groups: Vec<&str>) -> UserModel {
    UserModel {
        id: id.into(),
        handle: frona::core::Handle::try_new(id).unwrap(),
        email: format!("{id}@test"),
        name: id.into(),
        password_hash: String::new(),
        timezone: None,
        groups: groups.into_iter().map(String::from).collect(),
        deactivated_at: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn authorize_user_admin_can_list_users() {
    let (_db, service) = setup().await;
    let admin = test_user("admin-1", vec!["admins"]);
    let decision = service
        .authorize_user(&admin, PolicyAction::ListUsers)
        .await
        .unwrap();
    assert!(decision.allowed, "admin should be permitted to list_users");
}

#[tokio::test]
async fn authorize_user_admin_can_manage_users() {
    let (_db, service) = setup().await;
    let admin = test_user("admin-1", vec!["admins"]);
    let decision = service
        .authorize_user(
            &admin,
            PolicyAction::ManageUsers {
                target_user_id: "target".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        decision.allowed,
        "admin should be permitted to manage_users"
    );
}

#[tokio::test]
async fn authorize_user_member_cannot_list_users() {
    let (_db, service) = setup().await;
    let member = test_user("member-1", vec![]);
    let decision = service
        .authorize_user(&member, PolicyAction::ListUsers)
        .await
        .unwrap();
    assert!(
        !decision.allowed,
        "non-admin should not be permitted to list_users"
    );
}

#[tokio::test]
async fn authorize_user_member_cannot_manage_users() {
    // Regression for privilege-escalation footgun: even when target == self,
    // a non-admin must not be permitted to manage_users.
    let (_db, service) = setup().await;
    let member = test_user("member-1", vec![]);
    let decision = service
        .authorize_user(
            &member,
            PolicyAction::ManageUsers {
                target_user_id: "member-1".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        !decision.allowed,
        "non-admin must not self-promote via manage_users"
    );
}

#[tokio::test]
async fn authorize_user_admin_in_custom_group_still_admin() {
    // Membership in `admins` is the gate; additional groups don't break the permit.
    let (_db, service) = setup().await;
    let admin = test_user("admin-1", vec!["admins", "ops"]);
    let decision = service
        .authorize_user(
            &admin,
            PolicyAction::ManageUsers {
                target_user_id: "target".into(),
            },
        )
        .await
        .unwrap();
    assert!(decision.allowed);
}

mod decision_cache {
    use super::*;
    use frona::core::repository::Repository;

    fn invoke(tool: &str, group: &str) -> PolicyAction {
        PolicyAction::InvokeTool {
            tool_name: tool.into(),
            tool_group: group.into(),
        }
    }

    #[tokio::test]
    async fn repeat_call_with_same_args_hits_cache() {
        let (_db, service) = setup().await;
        let agent = test_agent("agent-1");

        service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        assert_eq!(service.decision_cache_entry_count().await, 1);

        service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        assert_eq!(
            service.decision_cache_entry_count().await,
            1,
            "second call must reuse the cached entry"
        );
    }

    #[tokio::test]
    async fn distinct_actions_cache_separately() {
        let (_db, service) = setup().await;
        let agent = test_agent("agent-1");

        service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        service
            .authorize("user-1", &agent, invoke("web_search", "search"))
            .await
            .unwrap();
        service
            .authorize("user-1", &agent, invoke("make_voice_call", "voice"))
            .await
            .unwrap();

        assert_eq!(service.decision_cache_entry_count().await, 3);
    }

    #[tokio::test]
    async fn distinct_agents_cache_separately() {
        let (_db, service) = setup().await;
        let agent_a = test_agent("agent-a");
        let agent_b = test_agent("agent-b");

        service
            .authorize("user-1", &agent_a, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        service
            .authorize("user-1", &agent_b, invoke("browser_navigate", "browser"))
            .await
            .unwrap();

        assert_eq!(service.decision_cache_entry_count().await, 2);
    }

    #[tokio::test]
    async fn invalidate_cache_for_user_drops_decisions() {
        let (_db, service) = setup().await;
        let agent = test_agent("agent-1");

        service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        service
            .authorize("user-1", &agent, invoke("web_search", "search"))
            .await
            .unwrap();
        assert_eq!(service.decision_cache_entry_count().await, 2);

        service.invalidate_cache("user-1").await;
        assert_eq!(service.decision_cache_entry_count().await, 0);
    }

    #[tokio::test]
    async fn invalidate_all_caches_drops_decisions() {
        let (_db, service) = setup().await;
        let agent = test_agent("agent-1");

        service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        assert_eq!(service.decision_cache_entry_count().await, 1);

        service.invalidate_all_caches();
        assert_eq!(service.decision_cache_entry_count().await, 0);
    }

    #[tokio::test]
    async fn create_policy_invalidates_decisions_so_next_call_sees_new_rule() {
        let (_db, service) = setup().await;
        let agent = test_agent("agent-1");

        // Prime: base policies allow.
        let allow_first = service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        assert!(allow_first.allowed);
        assert_eq!(service.decision_cache_entry_count().await, 1);

        // Adding a forbid policy invalidates the user's cache.
        service
            .create_policy(
                "user-1",
                "@id(\"deny-browser\")\nforbid(\n  principal == Policy::Agent::\"user-1/agent-1\",\n  action == Policy::Action::\"invoke_tool\",\n  resource in Policy::ToolGroup::\"browser\"\n);",
            )
            .await
            .unwrap();
        assert_eq!(
            service.decision_cache_entry_count().await,
            0,
            "create_policy must drop stale decisions"
        );

        let after = service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        assert!(
            after.is_denied(),
            "fresh evaluation must reflect the new forbid"
        );
    }

    #[tokio::test]
    async fn invalidate_for_one_user_keeps_other_users_cached() {
        let (db, service) = setup().await;
        let agent = test_agent("agent-1");

        service
            .authorize("user-1", &agent, invoke("browser_navigate", "browser"))
            .await
            .unwrap();

        let user_repo: SurrealRepo<frona::auth::User> = SurrealRepo::new(db.clone());
        user_repo
            .create(&frona::auth::User {
                id: "user-2".into(),
                handle: frona::handle!("user-2"),
                email: "u2@example.com".into(),
                name: "User Two".into(),
                password_hash: String::new(),
                timezone: None,
                groups: Vec::new(),
                deactivated_at: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let agent_u2 = Agent {
            user_id: "user-2".into(),
            ..test_agent("agent-1")
        };
        service
            .authorize("user-2", &agent_u2, invoke("browser_navigate", "browser"))
            .await
            .unwrap();
        assert_eq!(service.decision_cache_entry_count().await, 2);

        service.invalidate_cache("user-1").await;
        assert_eq!(
            service.decision_cache_entry_count().await,
            1,
            "user-2's entry must survive a user-1-scoped invalidate"
        );
    }
}

mod helpers;
