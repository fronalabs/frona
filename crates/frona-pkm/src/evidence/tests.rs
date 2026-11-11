use chrono::Utc;
use serde_json::json;

use super::*;

fn call(name: &str, arguments: Value, result: &str) -> ToolExecution {
    ToolExecution {
        id: "call-1".into(),
        chat_id: "chat-1".into(),
        message_id: "agent-1".into(),
        turn: 1,
        name: name.into(),
        arguments,
        result: result.into(),
        success: true,
        user_response: false,
        created_at: Utc::now(),
    }
}

#[test]
fn classifies_recall_mutation_sensitive_and_unknown_tools() {
    let memory_path = |path: &str| path.contains("/Memory/");
    assert_eq!(
        classify_execution(&call("memory_search", json!({}), "x"), memory_path),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call("read", json!({"path":"/vault/Memory/people/me.md"}), "x"),
            memory_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(&call("remember", json!({"content":"x"}), "ok"), memory_path),
        ExecutionEvidenceClass::MemoryMutation
    );
    assert_eq!(
        classify_execution(
            &call("memory_cite", json!({"path":"people/me"}), "x"),
            memory_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call("Memory.Search", json!({"query":"x"}), "x"),
            memory_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call("memory_remember", json!({"content":"x"}), "ok"),
            memory_path
        ),
        ExecutionEvidenceClass::MemoryMutation
    );
    assert_eq!(
        classify_execution(
            &call("store_user_memory", json!({"content":"x"}), "ok"),
            memory_path
        ),
        ExecutionEvidenceClass::MemoryMutation
    );
    assert_eq!(
        classify_execution(&call("request_credentials", json!({}), "ok"), memory_path),
        ExecutionEvidenceClass::Sensitive
    );
    assert_eq!(
        classify_execution(&call("custom_mcp_tool", json!({}), "ok"), memory_path),
        ExecutionEvidenceClass::Evidence
    );
}

#[test]
fn uses_the_host_memory_path_policy() {
    // The host supplies its storage-specific classification policy.
    let user_pkm_path = |path: &str| {
        path.contains("/users/testuser/pkm/") && !path.split('/').any(|part| part == "..")
    };

    assert_eq!(
        classify_execution(
            &call(
                "read",
                json!({"path":"/app/data/users/testuser/pkm/Memory/update-firmware.md"}),
                "existing memory",
            ),
            user_pkm_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call(
                "read",
                json!({"path":"/example/data/users/testuser/pkm/User Notes/source.md"}),
                "current PKM content",
            ),
            user_pkm_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call(
                "read",
                json!({"path":"/app/data/users/other/pkm/Memory/update-firmware.md"}),
                "another user's memory",
            ),
            user_pkm_path
        ),
        ExecutionEvidenceClass::Evidence
    );
    assert_eq!(
        classify_execution(
            &call(
                "read",
                json!({"path":"/app/data/users/testuser/files/update-firmware.md"}),
                "ordinary user file",
            ),
            user_pkm_path
        ),
        ExecutionEvidenceClass::Evidence
    );
    assert_eq!(
        classify_execution(
            &call(
                "read",
                json!({"path":"/app/data/users/testuser/pkm/../files/update-firmware.md"}),
                "path outside PKM storage",
            ),
            user_pkm_path
        ),
        ExecutionEvidenceClass::Evidence
    );
}

#[test]
fn mcpctl_is_evidence_unless_its_command_reads_memory() {
    let memory_path = |path: &str| path.contains("/Memory/");
    assert_eq!(
        classify_execution(
            &call(
                "shell",
                json!({"command":"mcpctl call weather.forecast"}),
                "sunny"
            ),
            memory_path
        ),
        ExecutionEvidenceClass::Evidence
    );
    assert_eq!(
        classify_execution(
            &call(
                "shell",
                json!({"command":"mcpctl call files.read path=/vault/Memory/people/me.md"}),
                "x"
            ),
            memory_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call(
                "shell",
                json!({"command":"mcpctl call memory.search query=Acme"}),
                "x"
            ),
            memory_path
        ),
        ExecutionEvidenceClass::Recall
    );
    assert_eq!(
        classify_execution(
            &call(
                "shell",
                json!({"command":"mcpctl call memory.remember content=Acme"}),
                "ok"
            ),
            memory_path
        ),
        ExecutionEvidenceClass::MemoryMutation
    );
}

#[test]
fn sanitization_removes_secret_fields_recursively() {
    let sanitized = sanitize_arguments(&json!({
        "url":"https://example.test", "headers":{"authorization":"Bearer secret", "accept":"json"},
        "nested":{"cookie":"session=secret", "entity_id":"switch.office"}
    }));
    assert_eq!(
        sanitized,
        json!({
            "url":"https://example.test", "headers":{"accept":"json"},
            "nested":{"entity_id":"switch.office"}
        })
    );
}

#[test]
fn web_page_evidence_uses_the_requested_url_and_not_result_text() {
    let requested = call(
        "web_fetch",
        json!({"url":"https://example.test/releases?version=4.2&channel=stable"}),
        "The result links to https://unrelated.test/redirect-target.",
    );
    let projection = ToolEvidenceProjection::new(
        &[requested],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let projected = projection.qualified_for_message("agent-1")[0];

    assert_eq!(
        projected.url.as_deref(),
        Some("https://example.test/releases?version=4.2&channel=stable")
    );
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::WebPage { url: Some(url), .. }
            if url == "https://example.test/releases?version=4.2&channel=stable"
    ));

    let result_only = call(
        "web_fetch",
        json!({}),
        "The result contains https://unrelated.test/result-only.",
    );
    let projection = ToolEvidenceProjection::new(
        &[result_only],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let projected = projection.qualified_for_message("agent-1")[0];

    assert_eq!(projected.url, None);
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::ToolResult { .. }
    ));

    let search = call(
        "web_search",
        json!({"query":"Acme 4.2 release"}),
        "Acme 4.2: https://example.test/releases/4.2",
    );
    let projection = ToolEvidenceProjection::new(
        &[search],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let projected = projection.qualified_for_message("agent-1")[0];

    assert_eq!(projected.url, None);
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::WebSearch { url: None, .. }
    ));
}

#[test]
fn curl_evidence_uses_the_full_requested_url() {
    let curl = call(
        "shell",
        json!({"command":"curl -L 'https://example.test/releases?version=4.2&channel=stable'"}),
        "Acme released version 4.2.",
    );
    let projection = ToolEvidenceProjection::new(
        &[curl],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let projected = projection.qualified_for_message("agent-1")[0];

    assert_eq!(
        projected.url.as_deref(),
        Some("https://example.test/releases?version=4.2&channel=stable")
    );
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::WebPage { .. }
    ));

    let ordinary_shell = call(
        "shell",
        json!({"command":"echo https://example.test/not-requested"}),
        "https://example.test/not-requested",
    );
    let projection = ToolEvidenceProjection::new(
        &[ordinary_shell],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let projected = projection.qualified_for_message("agent-1")[0];

    assert_eq!(projected.url, None);
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::ToolResult { .. }
    ));
}

#[test]
fn browser_content_evidence_inherits_the_requested_navigation_url() {
    let mut navigate = call(
        "browser_navigate",
        json!({"url":"https://example.test/docs?page=2&language=en"}),
        r#"{"title":"Documentation"}"#,
    );
    navigate.id = "navigate".into();
    let mut markdown = call(
        "browser_get_markdown",
        json!({"page":1}),
        "The documentation says Acme 4.2 is stable.",
    );
    markdown.id = "markdown".into();
    markdown.turn = 2;
    let mut click = call("browser_click", json!({"selector":"a.next"}), "clicked");
    click.id = "click".into();
    click.turn = 3;
    let mut extract = call(
        "browser_extract",
        json!({"selector":"main"}),
        "Content after a possible navigation.",
    );
    extract.id = "extract".into();
    extract.turn = 4;
    let projection = ToolEvidenceProjection::new(
        &[navigate, markdown, click, extract],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let calls = projection.qualified_for_message("agent-1");
    let projected = calls
        .iter()
        .find(|call| call.tool_call_id == "markdown")
        .unwrap();

    assert_eq!(
        projected.url.as_deref(),
        Some("https://example.test/docs?page=2&language=en")
    );
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::WebPage { .. }
    ));
    let projected = calls
        .iter()
        .find(|call| call.tool_call_id == "extract")
        .unwrap();
    assert_eq!(
        projected.url, None,
        "a possibly navigating action clears the old browser URL"
    );
    assert!(matches!(
        projected.support_citation(),
        ToolSupportCitation::ToolResult { .. }
    ));
}

#[test]
fn strong_match_requires_normalized_near_verbatim_containment() {
    assert!(normalized_containment(
        "Acme released version 4.2.",
        "Release notice: ACME released version 4.2!",
    ));
    assert!(!normalized_containment(
        "The deployment did not succeed.",
        "The deployment succeeded.",
    ));
    assert!(missing_critical_values("Acme released 4.2", "Acme 4.2 is available").is_empty());
    assert_eq!(
        missing_critical_values("Acme released 4.2", "Acme 4.3 is available"),
        vec!["4.2"]
    );
    assert!(
        missing_critical_values(
            "Used non-refurbished Accelerator A cards cost $15,000–$28,000.",
            "Accelerator A: used as-is, no refurbishment, $15,000–$28,000.",
        )
        .is_empty()
    );
    assert_eq!(
        missing_critical_values(
            "Kimi K2.7 Code has a 262K-token context window.",
            "kimi-k2.7-code context window: 262,144 tokens",
        ),
        vec!["262K-token"]
    );
    assert!(!normalized_containment(
        "The switch is off.",
        "POST switch.turn_off returned HTTP 200",
    ));

    let request_only = call("shell", json!({"command":"The switch is off"}), "HTTP 200");
    let projection = ToolEvidenceProjection::new(
        &[request_only],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    assert!(
        projection
            .strong_match_for_message("agent-1", "The switch is off")
            .is_none(),
        "a requested operation is available for fallback review but cannot prove its own outcome"
    );
}

#[test]
fn critical_value_diagnostics_ignore_presentation_and_report_every_missing_value() {
    let missing = missing_critical_values(
        "Flights EX101 and EXB303 depart at 9:00 AM and 2:30 PM.",
        "Flight EXA101 departs at 09:00AM.",
    );

    assert_eq!(missing, vec!["EX101", "EXB303", "2:30"]);
}

#[test]
fn evidence_horizon_crosses_windows_but_never_looks_forward() {
    let mut first = call(
        "web_fetch",
        json!({"url":"https://example.test/release"}),
        "Acme released 4.2",
    );
    first.id = "old-call".into();
    first.message_id = "agent-old".into();
    let mut future = call(
        "web_fetch",
        json!({"url":"https://example.test/future"}),
        "Acme released 5.0",
    );
    future.id = "future-call".into();
    future.message_id = "agent-future".into();
    let projection = ToolEvidenceProjection::new(
        &[first, future],
        &[
            "agent-old".into(),
            "agent-current".into(),
            "agent-future".into(),
        ],
        &["agent-current".into()],
        10,
        4_000,
        |_| false,
    );
    let calls = projection.qualified_for_message("agent-current");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_call_id, "old-call");
}

#[test]
fn completed_task_tree_tools_share_the_parent_assertion_scope() {
    let mut direct = call(
        "web_fetch",
        json!({"url":"https://example.test/model-alpha"}),
        "Model Alpha V1 needs about 16 GB for INT4 weights.",
    );
    direct.id = "direct-task-call".into();
    direct.chat_id = "task-chat".into();
    direct.message_id = "task-agent".into();
    let mut nested = call(
        "web_fetch",
        json!({"url":"https://example.test/accelerator-b"}),
        "Accelerator B has 48 GB of HBM3e memory.",
    );
    nested.id = "nested-task-call".into();
    nested.chat_id = "nested-task-chat".into();
    nested.message_id = "nested-task-agent".into();
    let mut unrelated = call(
        "web_fetch",
        json!({"url":"https://example.test/private"}),
        "Unrelated task result.",
    );
    unrelated.id = "unrelated-task-call".into();
    unrelated.chat_id = "other-task-chat".into();
    unrelated.message_id = "other-task-agent".into();
    let linked = HashMap::from([("parent-agent".to_string(), vec![direct, nested])]);

    let projection = ToolEvidenceProjection::new_with_task_evidence(
        &[unrelated],
        &["parent-agent".into()],
        &["parent-agent".into()],
        &linked,
        10,
        4_000,
        |_| false,
    );

    let calls = projection.qualified_for_message("parent-agent");
    assert_eq!(calls.len(), 2);
    assert!(projection.has_direct_evidence("parent-agent"));
    assert!(
        calls
            .iter()
            .any(|call| call.tool_call_id == "direct-task-call")
    );
    assert!(
        calls
            .iter()
            .any(|call| call.tool_call_id == "nested-task-call")
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.tool_call_id == "unrelated-task-call")
    );
    let result =
        projection.search_for_message("m7", "parent-agent", "Accelerator B 48 GB HBM3e", 4_000);
    assert!(result.contains("48 GB"));
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    let evidence_id = result["results"][0]["evidence_id"].as_str().unwrap();
    let selected = projection
        .resolve_evidence_id("m7", "parent-agent", evidence_id)
        .unwrap();
    assert_eq!(selected.call.tool_call_id, "nested-task-call");
}

#[test]
fn evidence_search_does_not_substitute_related_results_for_missing_critical_values() {
    let mut accelerator_a = call(
        "web_search",
        json!({"query":"Accelerator A memory bandwidth"}),
        "The Accelerator A has 32 GB of memory and 3.35 TB/s bandwidth.",
    );
    accelerator_a.id = "accelerator-a-call".into();
    let mut spark = call(
        "web_search",
        json!({"query":"Compute Box memory"}),
        "Compute Box has 64 GB of unified memory and 100 GB/s bandwidth.",
    );
    spark.id = "spark-call".into();
    let projection = ToolEvidenceProjection::new(
        &[accelerator_a, spark],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );

    let result = projection.search_for_message(
        "m9",
        "agent-1",
        "Accelerator B has 48 GB HBM3e and 4.8 TB/s bandwidth",
        4_000,
    );

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result).unwrap()["results"],
        json!([]),
        "related Accelerator A and Compute Box evidence must not replace a missing Accelerator B match",
    );
}

#[test]
fn evidence_search_ranks_results_when_at_least_one_critical_value_matches() {
    let update_procedure = call(
        "web_search",
        json!({"query":"Device X firmware update procedure"}),
        "The DX100 update procedure uses a USB-C data cable. Hold SET while connecting the cable, wait for DFU, then flash the firmware.",
    );
    let projection = ToolEvidenceProjection::new(
        &[update_procedure],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );

    let result = projection.search_for_message(
            "m2",
            "agent-1",
            "DX100 firmware update process latest version V1.71 DX100V171.hex DFU SET USB-C copy hex virtual drive",
            4_000,
        );
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(!result["results"].as_array().unwrap().is_empty());
    assert!(
        result["results"][0]["text"]
            .as_str()
            .unwrap()
            .contains("DFU")
    );
}

#[test]
fn recall_does_not_veto_independent_tool_evidence() {
    let mut recall = call("memory_search", json!({"query":"Acme release"}), "Acme 4.2");
    recall.id = "recall".into();
    let mut web = call(
        "web_search",
        json!({"query":"Acme release"}),
        "Acme released version 4.2",
    );
    web.id = "web".into();
    let projection = ToolEvidenceProjection::new(
        &[recall, web],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let calls = projection.qualified_for_message("agent-1");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].tool_call_id, "web");
}

#[test]
fn json_array_records_are_independent_evidence_chunks() {
    let chunks = evidence_chunks(
        r#"[{"service":"api","state":"healthy"},{"service":"worker","state":"degraded"}]"#,
    );
    assert_eq!(chunks.len(), 2);
    assert!(chunks[0].contains("api") && !chunks[0].contains("worker"));
}

#[test]
fn fused_ranking_is_stable_and_coverage_never_becomes_acceptance() {
    let mut unrelated = call(
        "web_search",
        json!({"query":"deploy"}),
        "deployment status unknown",
    );
    unrelated.id = "first".into();
    let mut relevant = call(
        "web_search",
        json!({"query":"deploy prod"}),
        "prod deployment green checks passed",
    );
    relevant.id = "second".into();
    relevant.turn = 2;
    let projection = ToolEvidenceProjection::new(
        &[unrelated, relevant],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );
    let first = projection.ranked_for_message(
        "agent-1",
        "production deployment is green",
        "prod deployment green",
    );
    let second = projection.ranked_for_message(
        "agent-1",
        "production deployment is green",
        "prod deployment green",
    );
    assert_eq!(
        first
            .iter()
            .map(|call| &call.tool_call_id)
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|call| &call.tool_call_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(first[0].tool_call_id, "second");
    assert!(coverage_score("production deployment is green", "prod deployment green") > 0.0);
    assert!(!normalized_containment(
        "production deployment is green",
        "prod deployment green"
    ));
    assert!(projection.qualified_for_message("another-agent").is_empty());
}

#[test]
fn assertion_scoped_search_excludes_future_and_zero_score_executions() {
    let mut relevant = call(
        "web_fetch",
        json!({"url":"https://example.test/deploy"}),
        "production deployment green",
    );
    relevant.id = "relevant".into();
    relevant.message_id = "agent-old".into();
    let mut unrelated = call(
        "web_fetch",
        json!({"url":"https://example.test/vitamins"}),
        "daily vitamin ingredients",
    );
    unrelated.id = "unrelated".into();
    unrelated.message_id = "agent-old".into();
    let mut future = call(
        "web_fetch",
        json!({"url":"https://example.test/future"}),
        "production deployment green",
    );
    future.id = "future".into();
    future.message_id = "agent-future".into();
    let projection = ToolEvidenceProjection::new(
        &[relevant, unrelated, future],
        &[
            "agent-old".into(),
            "agent-current".into(),
            "agent-future".into(),
        ],
        &["agent-current".into(), "agent-future".into()],
        10,
        4_000,
        |_| false,
    );

    let result =
        projection.search_for_message("m2", "agent-current", "production deployment", 4_000);

    assert!(result.contains("\"message\": \"m2\""));
    assert!(result.contains("\"evidence_id\": \"e1:chunk1\""));
    assert!(!result.contains("\"execution\""));
    assert!(!result.contains("chunk_id"));
    assert!(!result.contains("support"));
    assert!(!result.contains("future"));
    assert!(!result.contains("vitamin"));
    let selected = projection
        .resolve_evidence_id("m2", "agent-current", "e1:chunk1")
        .expect("returned evidence ID resolves inside the same Agent-message scope");
    assert_eq!(selected.call.tool_call_id, "relevant");
    assert!(
        projection
            .resolve_evidence_id("m3", "agent-current", "e1:chunk1")
            .is_some()
    );
}

#[test]
fn one_execution_keeps_one_evidence_id_across_search_messages() {
    let mut research = call(
        "web_fetch",
        json!({"url":"https://example.test/compute-box"}),
        "Compute Box has 64 GB of unified memory.",
    );
    research.message_id = "agent-research".into();
    let projection = ToolEvidenceProjection::new(
        &[research],
        &["agent-research".into(), "agent-summary".into()],
        &["agent-research".into(), "agent-summary".into()],
        10,
        4_000,
        |_| false,
    );
    let handles = HashMap::from([
        ("agent-research".to_string(), "m4".to_string()),
        ("agent-summary".to_string(), "m7".to_string()),
    ]);

    let direct = projection.search_for_message_with_handles(
        "m4",
        "agent-research",
        "Compute Box 64 GB",
        4_000,
        &handles,
    );
    let later = projection.search_for_message_with_handles(
        "m7",
        "agent-summary",
        "Compute Box 64 GB",
        4_000,
        &handles,
    );

    let direct: serde_json::Value = serde_json::from_str(&direct).unwrap();
    let later: serde_json::Value = serde_json::from_str(&later).unwrap();
    assert_eq!(
        direct["results"][0]["evidence_id"], later["results"][0]["evidence_id"],
        "an evidence ID identifies the execution chunk, not the search message",
    );
    assert_eq!(later["results"][0]["source_message"], "m4");
}

#[test]
fn search_returns_multiple_citable_chunks_from_one_execution() {
    let tool = call(
        "web_fetch",
        json!({"url":"https://example.test/status"}),
        r#"[{"service":"api","state":"healthy"},{"service":"worker","state":"degraded"}]"#,
    );
    let projection = ToolEvidenceProjection::new(
        &[tool],
        &["agent-1".into()],
        &["agent-1".into()],
        10,
        4_000,
        |_| false,
    );

    let result = projection.search_for_message("m2", "agent-1", "service state", 4_000);

    assert!(result.contains("e1:chunk1"));
    assert!(result.contains("e1:chunk2"));
}
