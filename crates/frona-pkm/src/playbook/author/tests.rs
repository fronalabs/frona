use super::*;
use crate::playbook::{ConversationSnapshot, Invocation, TranscriptMessage, tests::*};
use serde_json::{Value, json};
fn authored() -> Value {
    json!({"name":"Restart service","description":"Restart and verify the service.","body":"# Restart service\n\nRestart the service, then verify its status.","related_playbooks":[]})
}
fn input() -> PlaybookAuthorInput {
    PlaybookAuthorInput {
        path: "playbooks/target".into(),
        page: entity("playbooks/target"),
        memories: vec![memory("memory-one")],
        knowledge: KnowledgeSnapshot::default(),
        links: vec![],
        redirects: BTreeMap::new(),
        layout: PageLayout::default(),
        prefixes: PrefixMap::standard(),
    }
}
#[tokio::test]
async fn incomplete_content_is_repaired_before_canonical_projection() {
    let mut invalid = authored();
    invalid["body"] = json!("");
    let (harness, requests) = harness(vec![submit(invalid), submit(authored())]);
    let phase = PlaybookAuthor::new(harness).submission_attempts(2);
    let output = crate::Phase::run(&phase, input()).await.unwrap();
    assert_eq!(output.name, "Restart service");
    assert!(output.content.contains("Restart the service"));
    assert_eq!(output.rev, sha256_hex(&output.content));
    assert_eq!(output.path, "playbooks/target");
    assert!(crate::test_support::messages(&requests)[1].contains("missing: body"));
}
#[tokio::test]
async fn unaccepted_content_cannot_become_a_page() {
    let mut invalid = authored();
    invalid["body"] = json!("");
    let (harness, _) = harness(vec![submit(invalid)]);
    assert!(
        PlaybookAuthor::new(harness)
            .submission_attempts(1)
            .run(input())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn stale_path_is_rejected_before_model_execution() {
    let (harness, requests) = harness(vec![]);
    let mut input = input();
    input.path = "old/path".into();
    assert!(
        PlaybookAuthor::new(harness)
            .run(input)
            .await
            .unwrap_err()
            .to_string()
            .contains("stale path")
    );
    assert!(crate::test_support::messages(&requests).is_empty());
}
#[tokio::test]
async fn cancellation_never_returns_authored_content() {
    let (harness, requests) = harness(vec![submit(authored())]);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let error = PlaybookAuthor::new(harness)
        .run_with(input(), |run| run.cancellation(token))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled(_)));
    assert!(crate::test_support::messages(&requests).is_empty());
}
#[tokio::test]
async fn evidence_tools_are_scoped_redacted_and_include_intermediate_turns() {
    let (harness, requests) = harness(vec![
        vec![
            call("get_tool_output", json!({"id":"c1"})),
            call(
                "read_transcript",
                json!({"cursor":"t2","direction":"before","limit":20}),
            ),
        ],
        submit(authored()),
    ]);
    let mut input = input();
    input.memories[0].evidence = vec![crate::model::MemoryEvidence {
        strength: crate::model::EvidenceStrength::Explicit,
        source: EvidenceSource::AgentMessage {
            message_id: "agent-1".into(),
            agent_id: "agent".into(),
            chat_id: "chat-1".into(),
            quote: "Restarted service".into(),
        },
    }];
    input.knowledge.conversations.insert(
        "chat-1".into(),
        ConversationSnapshot {
            messages: vec![
                TranscriptMessage {
                    id: "user-1".into(),
                    role: MessageRole::User,
                    content: "Restart the service".into(),
                },
                TranscriptMessage {
                    id: "agent-1".into(),
                    role: MessageRole::Agent,
                    content: "Restarted service".into(),
                },
            ],
            invocations: vec![Invocation {
                id: "invocation-1".into(),
                message_id: "agent-1".into(),
                name: "shell".into(),
                arguments: json!({"command":"restart service","api_key":"private-value"}),
                result: "Service active".into(),
                turn: 1,
                created_at: chrono::Utc::now(),
                turn_text: Some("Checking service status".into()),
            }],
        },
    );
    assert!(PlaybookAuthor::new(harness).run(input).await.is_ok());
    let requests = crate::test_support::messages(&requests);
    assert!(requests[0].contains("Checking service status"));
    assert!(requests[1].contains("Service active"));
    assert!(!requests.join("\n").contains("private-value"));
}
#[tokio::test]
async fn related_playbooks_and_links_follow_canonical_redirects() {
    let mut authored = authored();
    authored["related_playbooks"] = json!(["old/path", "unknown/path"]);
    authored["body"] = json!("# Restart\n\nSee [[old/path]].");
    let (harness, _) = harness(vec![submit(authored)]);
    let mut input = input();
    input.redirects.insert("old/path".into(), "new/path".into());
    input.knowledge.entities.push(entity("new/path"));
    let output = PlaybookAuthor::new(harness).run(input).await.unwrap();
    assert_eq!(output.related_playbooks, ["new/path"]);
    assert!(!output.body.contains("old/path"));
    assert!(output.body.contains("new/path"));
}

#[tokio::test]
async fn research_reads_follow_canonical_redirects() {
    let (harness, requests) = harness(vec![
        vec![
            call("read_entity", json!({"path":"old/path"})),
            call("read_playbook", json!({"path":"old/path"})),
        ],
        submit(authored()),
    ]);
    let mut input = input();
    input
        .redirects
        .insert("old/path".into(), "middle/path".into());
    input
        .redirects
        .insert("middle/path".into(), "playbooks/target".into());
    PlaybookAuthor::new(harness).run(input).await.unwrap();
    let requests = crate::test_support::messages(&requests);
    assert!(requests[1].contains("path=playbooks/target"));
    assert!(!requests[1].contains("Unknown Playbook path"));
    assert!(!requests[1].contains("No entity exists"));
}
