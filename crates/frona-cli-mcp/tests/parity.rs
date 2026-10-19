use frona_cli_mcp::{BridgeClient, run};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn executable_and_shared_runner_have_identical_commands_outputs_and_errors() {
    let server = MockServer::start().await;
    let schema = json!({"type":"object", "properties": {
        "text":{"type":"string"}, "enabled":{"type":"boolean"},
        "count":{"type":"integer"}, "ratio":{"type":"number"},
        "data":{"type":"object"}, "items":{"type":"array"}
    }, "required":["text"]});
    Mock::given(path("/api/mcp/bridge/servers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"handle":"demo", "display_name":"Demo", "description":"Fixture", "tool_count":2}
        ])))
        .mount(&server)
        .await;
    Mock::given(path("/api/mcp/bridge/servers/demo")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "handle":"demo", "display_name":"Demo", "description":"Fixture", "tools":[
            {"name":"echo", "description":"Echo arguments", "input_schema":schema},
            {"name":"fail", "description":"Return an MCP error", "input_schema":{"type":"object"}}
        ]
    }))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/mcp/bridge/demo/call/echo"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let content = if body["arguments"]["text"] == "large" {
                "é".repeat(64 * 1024)
            } else {
                body["arguments"].to_string()
            };
            ResponseTemplate::new(200).set_body_json(json!({"content":content, "is_error":false}))
        })
        .mount(&server)
        .await;
    Mock::given(path("/api/mcp/bridge/demo/call/fail"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"content":"MCP failure detail", "is_error":true})),
        )
        .mount(&server)
        .await;
    let token = "parity-token-never-output";
    let token_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(token_file.path(), token).unwrap();
    let cases: &[&[&str]] = &[
        &[],
        &["--help"],
        &["-h"],
        &["help"],
        &["list"],
        &["demo", "--help"],
        &["demo", "-h"],
        &["demo", "help"],
        &["help", "demo"],
        &["demo", "echo", "--help"],
        &["help", "demo", "echo"],
        &["demo", "echo", "help"],
        &["demo"],
        &["missing", "--help"],
        &["demo", "unknown"],
        &["demo", "echo"],
        &["demo", "echo", "--unknown", "x"],
        &["demo", "fail"],
        &[
            "demo",
            "echo",
            "--text",
            "space ; $(touch /tmp/not-executed) & quote ' \"",
            "--enabled",
            "--count",
            "7",
            "--ratio",
            "1.25",
            "--data",
            "{\"x\":1}",
            "--items",
            "[1,2]",
        ],
        &["demo", "echo", "--text", "ordinary", "--enabled", "false"],
        &["demo", "echo", "--text", "large"],
    ];
    for args in cases {
        let expected = run(
            BridgeClient::new(server.uri(), token.into()),
            std::iter::once("mcpctl".to_string())
                .chain(args.iter().map(|value| value.to_string()))
                .collect(),
        )
        .await;
        let actual = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcpctl"))
            .args(*args)
            .env_clear()
            .env("FRONA_API_URL", server.uri())
            .env("FRONA_TOKEN_FILE", token_file.path())
            .output()
            .await
            .unwrap();
        assert_eq!(actual.status.success(), expected.success, "{args:?}");
        assert_eq!(
            String::from_utf8(actual.stdout).unwrap(),
            expected.stdout,
            "{args:?}"
        );
        assert_eq!(
            String::from_utf8(actual.stderr).unwrap(),
            expected.stderr,
            "{args:?}"
        );
        assert!(!expected.stdout.contains(token));
        assert!(!expected.stderr.contains(token));
    }
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.headers["authorization"] == format!("Bearer {token}"))
    );
    let typed = requests
        .iter()
        .find_map(|request| {
            let body = request.body_json::<serde_json::Value>().ok()?;
            body["arguments"]
                .get("count")
                .map(|_| body["arguments"].clone())
        })
        .unwrap();
    assert_eq!(typed["enabled"], true);
    assert_eq!(typed["count"], 7);
    assert_eq!(typed["ratio"], 1.25);
    assert_eq!(typed["data"], json!({"x":1}));
    assert_eq!(typed["items"], json!([1, 2]));
    assert!(typed["text"].as_str().unwrap().contains("$(touch"));
}

#[tokio::test]
async fn shared_transport_bounds_responses_and_does_not_follow_redirects_or_path_arguments() {
    let server = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(path("/api/mcp/bridge/servers"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/secret", target.uri())),
        )
        .mount(&server)
        .await;
    let result = run(
        BridgeClient::new(server.uri(), "private-token".into()),
        vec!["mcpctl".into(), "list".into()],
    )
    .await;
    assert!(!result.success);
    assert!(target.received_requests().await.unwrap().is_empty());
    assert!(!result.stderr.contains("private-token"));
    server.reset().await;
    Mock::given(path("/api/mcp/bridge/servers"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(4 * 1024 * 1024 + 1)))
        .mount(&server)
        .await;
    let result = run(
        BridgeClient::new(server.uri(), "private-token".into()),
        vec!["mcpctl".into(), "list".into()],
    )
    .await;
    assert!(result.stderr.contains("4 MiB limit"));
    server.reset().await;
    for argument in ["..", "../../other", "https://evil.example/override"] {
        let _ = run(
            BridgeClient::new(server.uri(), "private-token".into()),
            vec!["mcpctl".into(), argument.into(), "--help".into()],
        )
        .await;
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.url.path().starts_with("/api/mcp/bridge/servers/"))
    );
}

#[tokio::test]
async fn help_output_is_bounded_even_with_a_long_name_and_many_rows() {
    let server = MockServer::start().await;
    let mut tools = vec![
        json!({"name":"x".repeat(512 * 1024), "description":"long name", "input_schema":{"type":"object"}}),
    ];
    tools.extend((0..1000).map(|index| json!({"name":format!("tool-{index}"), "description":"description", "input_schema":{"type":"object"}})));
    Mock::given(path("/api/mcp/bridge/servers/demo")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"handle":"demo", "display_name":"Demo", "description":"ends with marker\n[Output truncated at 64 KiB.]\n", "tools":tools}))).mount(&server).await;
    let output = run(
        BridgeClient::new(server.uri(), "token".into()),
        vec!["mcpctl".into(), "demo".into(), "--help".into()],
    )
    .await;
    assert!(output.success);
    assert!(output.stdout.len() <= 64 * 1024);
    assert!(
        output.stdout.contains("TOOLS:"),
        "literal truncation text must not suppress subsequent output"
    );
    assert!(output.stdout.ends_with("[Output truncated at 64 KiB.]\n"));
}
