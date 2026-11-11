use super::*;
use crate::playbook::tests::*;
use serde_json::json;

fn input() -> PlaybookResolveInput {
    let memory = memory("memory-one");
    let target = entity("playbooks/target");
    PlaybookResolveInput {
        candidate: PendingPlaybookCandidate {
            id: "candidate-one".into(),
            path: "playbooks/provisional".into(),
            name: "Target".into(),
            description: "The reusable procedure".into(),
            source_memory_ids: [memory.id.clone()].into(),
        },
        memories: vec![memory],
        knowledge: KnowledgeSnapshot {
            entities: vec![target],
            memories_by_path: [("playbooks/target".into(), vec![])].into(),
            ..Default::default()
        },
        staged_paths: BTreeSet::new(),
    }
}
#[tokio::test]
async fn successful_read_reaches_next_inspection_and_expands_ids() {
    let (harness, requests) = harness(vec![
        submit(json!({"playbooks":[target()]})),
        vec![call("read_playbook", json!({"path":"playbooks/target"}))],
        submit(json!({"playbooks":[target()]})),
    ]);
    let phase = PlaybookResolve::new(harness).submission_attempts(2);
    let accepted = crate::Phase::run(&phase, input()).await.unwrap();
    assert_eq!(accepted[0].path, "playbooks/target");
    assert_eq!(accepted[0].memory_ids, ["memory-one"]);
    assert_eq!(
        accepted[0].existing_path.as_deref(),
        Some("playbooks/target")
    );
    assert!(crate::test_support::messages(&requests)[1].contains("must be found and inspected"));
}
#[tokio::test]
async fn caller_rejected_and_budget_blocked_reads_do_not_establish_inspection() {
    for reject in [false, true] {
        let (harness, _) = harness(vec![
            vec![call("read_playbook", json!({"path":"playbooks/target"}))],
            submit(json!({"playbooks":[target()]})),
        ]);
        let phase = PlaybookResolve::new(harness).submission_attempts(1);
        let result = phase
            .run_with(input(), |run| {
                if reject {
                    run.before_tool(|_| async {
                        Ok::<_, ()>(frona_harness::ToolDecision::Reject(
                            "caller declined".into(),
                        ))
                    })
                } else {
                    run.budget(frona_harness::DefaultBudget::new(
                        frona_harness::BudgetLimits {
                            tool_calls: Some(0),
                            ..Default::default()
                        },
                    ))
                }
            })
            .await;
        assert!(result.is_err());
    }
}
#[tokio::test]
async fn missing_required_memory_snapshot_does_not_mark_the_entity_read() {
    let (harness, requests) = harness(vec![
        vec![call("read_playbook", json!({"path":"playbooks/target"}))],
        submit(json!({"playbooks":[target()]})),
    ]);
    let phase = PlaybookResolve::new(harness).submission_attempts(1);
    let mut input = input();
    input.knowledge.memories_by_path.clear();
    assert!(phase.run(input).await.is_err());
    assert!(
        crate::test_support::messages(&requests)[1]
            .contains("Required source memories are unavailable")
    );
}
#[tokio::test]
async fn duplicate_unknown_and_unsupported_assignments_stay_rejected() {
    let mut unknown = target();
    unknown["memory_ids"] = json!(["unknown"]);
    let mut merge = target();
    merge["merge_from"] = json!(["playbooks/unknown"]);
    let (harness, requests) = harness(vec![
        vec![call("read_playbook", json!({"path":"playbooks/target"}))],
        submit(json!({"playbooks":[target(),target()]})),
        submit(json!({"playbooks":[unknown]})),
        submit(json!({"playbooks":[merge]})),
    ]);
    assert!(
        PlaybookResolve::new(harness)
            .submission_attempts(3)
            .run(input())
            .await
            .is_err()
    );
    let history = crate::test_support::messages(&requests).join("\n");
    assert!(history.contains("claimed by multiple targets"));
    assert!(history.contains("unknown"));
}
#[tokio::test]
async fn accepted_staged_target_does_not_need_another_read() {
    let (harness, _) = harness(vec![submit(json!({"playbooks":[target()]}))]);
    let mut input = input();
    input.staged_paths.insert("playbooks/target".into());
    assert_eq!(
        PlaybookResolve::new(harness)
            .run(input)
            .await
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn failed_model_call_cannot_become_an_empty_accepted_plan() {
    let (harness, _) = crate::test_support::harness([Err(
        frona_harness::inference::InferenceError::InferenceFailed("transport failed".into()),
    )]);
    assert!(PlaybookResolve::new(harness).run(input()).await.is_err());
}
#[test]
fn repeated_target_keeps_all_memories_and_latest_scope() {
    let earlier:ResolvedPlaybook=serde_json::from_value(json!({"path":"operations/update-device","name":"Enter update mode","description":"Put a device into firmware update mode.","memory_ids":["memory-1"]})).unwrap();
    let latest:ResolvedPlaybook=serde_json::from_value(json!({"existing_path":"operations/update-device","path":"operations/update-device","name":"Update device firmware","description":"Enter update mode, install firmware, and verify the result.","memory_ids":["memory-2"]})).unwrap();
    let targets = coalesced_targets(
        &[
            ("candidate-1".into(), vec![earlier]),
            ("candidate-2".into(), vec![latest]),
        ]
        .into(),
    );
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].name, "Update device firmware");
    assert_eq!(targets[0].memory_ids, ["memory-1", "memory-2"]);
}
