use frona_harness::inference::history_compat::{
    contextualize_legacy, legacy_call, reasoning_blocks,
};
use frona_harness::{HistoryDecodeErrorKind, UsageReport, decode_jsonl, encode_jsonl};
use rig_core::message::{AssistantContent, AssistantMessage, Message, UserContent};
use serde_json::{Value, json};

fn record(seq: u64, event: Value) -> Value {
    json!({"version":1,"conversation":"00000000-0000-0000-0000-000000000000","seq":seq,"event":event})
}
fn bytes(records: &[Value]) -> Vec<u8> {
    records
        .iter()
        .flat_map(|record| {
            let mut line = serde_json::to_vec(record).unwrap();
            line.push(b'\n');
            line
        })
        .collect()
}
fn opened(initial: Value) -> Value {
    record(
        0,
        json!({"Opened":{"initial":initial,"instructions":"original instructions","mode":"Text"}}),
    )
}
fn output(content: Value) -> Value {
    json!({"content":content,"structured":null,"usage":{"Reported":{"input_tokens":7,"output_tokens":3,"total_tokens":10,"cached_input_tokens":0,"cache_creation_input_tokens":0,"tool_use_prompt_tokens":0,"reasoning_tokens":0}},"metadata":{"opaque":{"type":"reasoning","content":"untouched"}},"ttft_ms":9})
}
fn call(id: &str, args: Value) -> Value {
    json!({"type":"toolcall","id":id,"function":{"name":"lookup","arguments":args},"signature":"signature+/=","additional_params":{"opaque":true}})
}

#[test]
fn version_one_keeps_media_calls_usage_and_application_payloads() {
    let args = json!({"type":"toolcall","id":"application-owned","data":{"type":"string","value":"literal"}});
    let records = vec![
        opened(
            json!([{"type":"document","data":{"type":"string","value":"old document text"},"media_type":"txt","additional_params":{"title":"Original"}}]),
        ),
        record(
            1,
            json!({"Response":{"response":0,"message":1,"origin":"google/gemini-2.5-pro","offered_tools":[],"output":output(json!([call("local-handle",args.clone())]))}}),
        ),
        record(
            2,
            json!({"ActionResolved":{"action":{"conversation":"00000000-0000-0000-0000-000000000000","response":0,"action":0},"resolution":{"Result":{"content":[{"type":"image","data":{"type":"base64","value":"aW1hZ2U="},"media_type":"png","additional_params":{"opaque-image":true}},{"type":"json","value":args}],"metadata":{"id":"untouched"}}}}}),
        ),
        record(
            3,
            json!({"Checkpoint":{"checkpoint":{"schema":"application","version":1,"payload":{"type":"reasoning","content":[{"type":"encrypted","content":"not-a-message"}]},"through_seq":2}}}),
        ),
    ];
    let decoded = decode_jsonl(&bytes(&records)).unwrap();
    let conversation = &decoded.replay.conversation;
    let Message::User { content } = &conversation.messages()[0] else {
        panic!()
    };
    let UserContent::Document(document) = &content[0] else {
        panic!()
    };
    assert_eq!(
        serde_json::to_value(&document.data).unwrap(),
        json!({"type":"text","value":"old document text"})
    );
    let response = conversation.responses()[0].output();
    let UsageReport::Reported(usage) = &response.usage else {
        panic!()
    };
    assert_eq!(usage.input_tokens, Some(7));
    assert_eq!(
        response.metadata,
        records[1]["event"]["Response"]["output"]["metadata"]
    );
    let AssistantContent::ToolCall(call) = &response.content[0] else {
        panic!()
    };
    assert!(call.id.is_local());
    assert_eq!(
        serde_json::to_value(&call.function.arguments).unwrap(),
        args
    );
    let native = response.content[0].native_item().unwrap();
    assert_eq!(native["thoughtSignature"], "signature+/=");
    assert_eq!(
        native["frona_legacy_additional_params"],
        json!({"opaque":true})
    );
    let Message::User { content } = &conversation.messages()[2] else {
        panic!()
    };
    let UserContent::ToolResult(result) = &content[0] else {
        panic!()
    };
    assert_eq!(call.id, result.call);
    assert_eq!(
        serde_json::to_value(&result.content[1]).unwrap()["value"],
        args
    );
    assert_eq!(
        decoded.replay.checkpoint.as_ref().unwrap().1.payload,
        records[3]["event"]["Checkpoint"]["checkpoint"]["payload"]
    );
    let encoded = encode_jsonl(conversation.history()).unwrap();
    let reread = decode_jsonl(&encoded).unwrap();
    assert_eq!(
        conversation.messages(),
        reread.replay.conversation.messages()
    );
    assert!(conversation.history().iter().all(|r| r.version == 2));
}

#[test]
fn legacy_call_mapping_is_stable_without_conflating_local_and_provider_ids() {
    let a = legacy_call(call("same", json!({}))).unwrap();
    let b = legacy_call(call("same", json!({}))).unwrap();
    let c = legacy_call(call("different", json!({}))).unwrap();
    assert_eq!(a.id, b.id);
    assert_ne!(a.id, c.id);
    let mut wire = call("same", json!({}));
    wire["provider"] = json!({"call_id":"same","item_id":"fc_original"});
    wire["signature"] = Value::Null;
    let provider = legacy_call(wire).unwrap();
    assert_ne!(a.id, provider.id);
    assert!(!provider.id.is_local());
    assert_eq!(provider.id.wire(), "same");
    let block = AssistantContent::ToolCall(provider);
    assert_eq!(block.native_item().unwrap()["id"], "fc_original");
}

#[test]
fn encrypted_reasoning_preserves_order_and_archives_provider_specific_combinations() {
    let blocks=reasoning_blocks(json!({"id":"rs_original","content":[{"type":"summary","content":"first"},{"type":"summary","content":"second"},{"type":"encrypted","content":"ciphertext+/="}]})).unwrap();
    assert_eq!(blocks.len(), 1);
    let native = blocks[0].native_item().unwrap();
    assert_eq!(native["id"], "rs_original");
    assert_eq!(native["summary"][0]["text"], "first");
    assert_eq!(native["summary"][1]["text"], "second");
    assert_eq!(native["encrypted_content"], "ciphertext+/=");
    let original = json!({"id":null,"content":[{"type":"encrypted","content":"a"},{"type":"encrypted","content":"b"}]});
    let mixed = reasoning_blocks(original.clone()).unwrap();
    assert_eq!(
        mixed[0].native_item().unwrap()["frona_legacy_reasoning"],
        original
    );
}

#[test]
fn legacy_signed_reasoning_uses_configured_wire_but_current_origins_are_untouched() {
    use frona_harness::{
        Handle,
        config::ProviderModel,
        inference::provider::{ModelConfig, ModelRequestSettings},
    };
    let model = ModelConfig {
        catalog_provider: "google".into(),
        provider_handle: Handle::const_validated("google"),
        model_id: "gemini-2.5-pro".into(),
        provider: ProviderModel::Gemini {
            params: Default::default(),
        },
        request_settings: ModelRequestSettings::default(),
    };
    let content=reasoning_blocks(json!({"id":null,"content":[{"type":"text","content":{"text":"thought","signature":"signed"}}]})).unwrap();
    let mut assistant = AssistantMessage::new(content);
    contextualize_legacy(&mut assistant, &model);
    assert_eq!(
        assistant.content[0].native_item().unwrap()["thoughtSignature"],
        "signed"
    );
    assert_eq!(
        assistant.origin.as_ref().unwrap().api.as_str(),
        "frona.legacy"
    );
    assistant.origin = Some(rig_core::message::Origin::new(
        "anthropic.messages",
        "anthropic",
        "claude",
    ));
    let before = assistant.clone();
    contextualize_legacy(&mut assistant, &model);
    assert_eq!(assistant, before);
}

#[test]
fn version_one_corruption_versions_and_unterminated_tail_keep_boundaries() {
    let first = opened(json!([{"type":"text","text":"hello"}]));
    let mut encoded = bytes(std::slice::from_ref(&first));
    let start = encoded.len();
    encoded.extend_from_slice(br#"{"version":1,"unfinished"#);
    let decoded = decode_jsonl(&encoded).unwrap();
    assert_eq!(decoded.incomplete_tail, Some(start..encoded.len()));
    encoded.push(b'\n');
    assert_eq!(
        decode_jsonl(&encoded).unwrap_err().kind,
        HistoryDecodeErrorKind::Corruption
    );
    let mut future = first.clone();
    future["version"] = json!(3);
    assert_eq!(
        decode_jsonl(&bytes(&[future])).unwrap_err().kind,
        HistoryDecodeErrorKind::UnsupportedVersion
    );
    let text = String::from_utf8(bytes(&[first]))
        .unwrap()
        .replace("\"seq\":0", "\"seq\":0,\"seq\":0");
    assert_eq!(
        decode_jsonl(text.as_bytes()).unwrap_err().kind,
        HistoryDecodeErrorKind::Format
    );
}

#[test]
fn version_one_compaction_keeps_summary_and_summarizer_provenance() {
    let records = vec![
        opened(json!([{"type":"text","text":"input"}])),
        record(
            1,
            json!({"Response":{"response":0,"message":1,"origin":"openai/answer-model","offered_tools":[],"output":output(json!([{"type":"text","text":"answer"}]))}}),
        ),
        record(
            2,
            json!({"User":{"message":2,"content":[{"type":"text","text":"continue"}]}}),
        ),
        record(
            3,
            json!({"Compacted":{"view":{"source_message_count":3,"entries":[{"Summary":{"message":{"role":"user","content":[{"type":"text","text":"summary"}]},"covered_original_positions":[0,1]}},{"Original":2}]},"summarizer":"openai/summary-model","output":output(json!([{"type":"text","text":"summary"}]))}}),
        ),
    ];
    let decoded = decode_jsonl(&bytes(&records)).unwrap();
    let frona_harness::HistoryEvent::Compacted {
        output, summarizer, ..
    } = &decoded.replay.conversation.history()[3].event
    else {
        panic!()
    };
    assert_eq!(summarizer, "openai/summary-model");
    assert_eq!(output.origin.as_ref().unwrap().model, "summary-model");
    let encoded = encode_jsonl(decoded.replay.conversation.history()).unwrap();
    assert!(decode_jsonl(&encoded).is_ok());
}

fn resolved_call_records(source: &str, call: Value) -> Vec<Value> {
    vec![
        opened(json!([{"type":"text","text":"look up the answer"}])),
        record(
            1,
            json!({"Response":{"response":0,"message":1,"origin":source,"offered_tools":[],"output":output(json!([call]))}}),
        ),
        record(
            2,
            json!({"ActionResolved":{"action":{"conversation":"00000000-0000-0000-0000-000000000000","response":0,"action":0},"resolution":{"Result":{"content":[{"type":"text","text":"answer found"}],"metadata":null}}}}),
        ),
    ]
}

#[tokio::test]
async fn version_one_gemini_signed_call_replays_without_archival_fields() {
    use frona_harness::{Model, ModelRequest};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let records = resolved_call_records(
        "google/mock-model",
        call("local-handle", json!({"query":"original"})),
    );
    let replay = decode_jsonl(&bytes(&records)).unwrap().replay;
    let archived = encode_jsonl(replay.conversation.history()).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "candidates":[{"content":{"role":"model","parts":[{"text":"done"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1,"totalTokenCount":11}
    }))).expect(1).mount(&server).await;
    let model = Model::builder("google", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .build()
        .unwrap();
    let mut request = ModelRequest::new("unused");
    request.messages = replay.conversation.messages().to_vec();
    request.tools.push(rig_core::completion::request::ToolDefinition {
        annotations: Default::default(),
        name: "lookup".try_into().unwrap(),
        description: "Look up the requested answer".into(),
        parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
    });
    model.complete(request).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let parts = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap());
    let signed = parts
        .into_iter()
        .find(|part| part.get("functionCall").is_some())
        .unwrap();
    assert_eq!(signed["thoughtSignature"], "signature+/=");
    assert_eq!(signed["functionCall"]["args"], json!({"query":"original"}));
    assert!(
        signed["functionCall"].get("id").is_none(),
        "local handle must not become a provider-issued id"
    );
    assert!(!body.to_string().contains("frona_legacy_additional_params"));
    assert_eq!(
        encode_jsonl(replay.conversation.history()).unwrap(),
        archived
    );
    assert!(
        String::from_utf8(archived)
            .unwrap()
            .contains("frona_legacy_additional_params")
    );
}

#[tokio::test]
async fn version_one_openai_dual_ids_replay_without_archival_fields() {
    use frona_harness::{Model, ModelRequest, config::ApiSurface};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let mut original = call("call_original", json!({"query":"original"}));
    original["signature"] = Value::Null;
    original["provider"] = json!({"call_id":"call_original","item_id":"fc_original"});
    let replay = decode_jsonl(&bytes(&resolved_call_records(
        "openai/mock-model",
        original,
    )))
    .unwrap()
    .replay;
    let archived = encode_jsonl(replay.conversation.history()).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_next","object":"response","created_at":0,"status":"completed",
            "output":[{"type":"message","id":"msg_next","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}],
            "usage":{"input_tokens":10,"output_tokens":1,"total_tokens":11}
        }))).expect(1).mount(&server).await;
    let model = Model::builder("openai", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .api(ApiSurface::Responses)
        .build()
        .unwrap();
    let mut request = ModelRequest::new("unused");
    request.messages = replay.conversation.messages().to_vec();
    request.tools.push(rig_core::completion::request::ToolDefinition {
        annotations: Default::default(),
        name: "lookup".try_into().unwrap(),
        description: "Look up the requested answer".into(),
        parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
    });
    model.complete(request).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let items = body["input"].as_array().unwrap();
    let call = items
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    assert_eq!(call["id"], "fc_original");
    assert_eq!(call["call_id"], "call_original");
    assert_eq!(
        serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
        json!({"query":"original"})
    );
    let result = items
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(result["call_id"], "call_original");
    assert!(!body.to_string().contains("frona_legacy_additional_params"));
    assert_eq!(
        encode_jsonl(replay.conversation.history()).unwrap(),
        archived
    );
}

#[tokio::test]
async fn context_bound_signed_legacy_fails_before_network_without_inventing_context() {
    use frona_harness::{Model, ModelRequest};
    let records = vec![
        opened(json!([{"type":"text","text":"original input"}])),
        record(
            1,
            json!({"Response":{"response":0,"message":1,"origin":"anthropic/claude-opus-5-5","offered_tools":[],"output":output(json!([
                {"type":"reasoning","id":null,"content":[{"type":"text","content":{"text":"private reasoning","signature":"original-signature"}}]},
                {"type":"text","text":"original answer"}
            ]))}}),
        ),
        record(
            2,
            json!({"User":{"message":2,"content":[{"type":"text","text":"continue"}]}}),
        ),
    ];
    let replay = decode_jsonl(&bytes(&records)).unwrap().replay;
    let archived = encode_jsonl(replay.conversation.history()).unwrap();
    let server = wiremock::MockServer::start().await;
    let model = Model::builder("anthropic", "claude-opus-5-5")
        .api_key("fixture-key")
        .base_url(server.uri())
        .build()
        .unwrap();
    let mut request = ModelRequest::new("unused");
    request.messages = replay.conversation.messages().to_vec();
    let error = model.complete(request).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("original prompt/tool context fingerprint was not recorded"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        encode_jsonl(replay.conversation.history()).unwrap(),
        archived
    );
}

#[test]
fn version_one_rejects_duplicate_nested_structural_fields() {
    let initial = opened(json!([{"type":"text","text":"hello"}]));
    let encoded = String::from_utf8(bytes(&[initial])).unwrap();
    for (needle, duplicate) in [
        ("\"initial\":", "\"initial\":[],\"initial\":"),
        ("\"mode\":", "\"mode\":\"Ephemeral\",\"mode\":"),
    ] {
        assert!(encoded.contains(needle));
        let duplicate = encoded.replacen(needle, duplicate, 1);
        let error = decode_jsonl(duplicate.as_bytes()).unwrap_err();
        assert_eq!(error.kind, HistoryDecodeErrorKind::Format);
        assert!(error.message.contains("duplicate field"));
    }
    let response = record(
        1,
        json!({"Response":{"response":0,"message":1,"origin":"openai/mock-model","offered_tools":[],"output":output(json!([{"type":"text","text":"done"}]))}}),
    );
    let encoded = String::from_utf8(bytes(&[
        opened(json!([{"type":"text","text":"hello"}])),
        response,
    ]))
    .unwrap();
    let duplicate = encoded.replacen("\"output\":", "\"output\":{},\"output\":", 1);
    let error = decode_jsonl(duplicate.as_bytes()).unwrap_err();
    assert_eq!(error.seq, 1);
    assert_eq!(error.kind, HistoryDecodeErrorKind::Format);
    assert!(error.message.contains("duplicate field"));
}

#[tokio::test]
async fn version_one_openrouter_replays_call_metadata_and_ordered_reasoning_details() {
    use frona_harness::{Model, ModelRequest};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    for full in [true, false] {
        let mut original = call("call_original", json!({"query":"original"}));
        original["provider"] = json!({"call_id":"call_original"});
        let expected = json!({"type":"reasoning.encrypted","id":"reasoning_original","format":"anthropic-claude-v1","index":null,"data":"signature+/="});
        original["additional_params"] = if full {
            expected.clone()
        } else {
            json!({"id":"reasoning_original","format":"anthropic-claude-v1"})
        };
        if full {
            original["signature"] = Value::Null;
        }
        let mut records = resolved_call_records("openrouter/mock-model", original);
        let reasoning = json!({"type":"reasoning","id":"rs_original","content":[
            {"type":"encrypted","content":"ciphertext-first"},
            {"type":"text","content":{"text":"thought","signature":"text-signature"}},
            {"type":"summary","content":"summary"},
            {"type":"encrypted","content":"ciphertext-last"}
        ]});
        records[1]["event"]["Response"]["output"]["content"]
            .as_array_mut()
            .unwrap()
            .insert(0, reasoning);
        let replay = decode_jsonl(&bytes(&records)).unwrap().replay;
        let archived = encode_jsonl(replay.conversation.history()).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"chat_next","object":"chat.completion","created":0,"model":"mock-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}
            }))).expect(1).mount(&server).await;
        let model = Model::builder("openrouter", "mock-model")
            .api_key("fixture-key")
            .base_url(server.uri())
            .build()
            .unwrap();
        let mut request = ModelRequest::new("unused");
        request.messages = replay.conversation.messages().to_vec();
        request
            .tools
            .push(rig_core::completion::request::ToolDefinition {
                annotations: Default::default(),
                name: "lookup".try_into().unwrap(),
                description: "Lookup".into(),
                parameters: json!({"type":"object","properties":{"query":{"type":"string"}}}),
            });
        model.complete(request).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        let details = assistant["reasoning_details"].as_array().unwrap();
        assert_eq!(details.len(), 5);
        assert_eq!(details[0]["data"], "ciphertext-first");
        assert_eq!(details[1]["signature"], "text-signature");
        assert_eq!(details[2]["summary"], "summary");
        assert_eq!(details[3]["data"], "ciphertext-last");
        for (index, detail) in details[..4].iter().enumerate() {
            assert_eq!(detail["id"], "rs_original");
            assert_eq!(detail["index"], index);
        }
        assert_eq!(details[4], expected);
        assert_eq!(assistant["tool_calls"][0]["id"], "call_original");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "lookup");
        assert!(!body.to_string().contains("frona_legacy_"));
        assert_eq!(
            encode_jsonl(replay.conversation.history()).unwrap(),
            archived
        );
    }
}

#[tokio::test]
async fn openrouter_local_call_reasoning_uses_collision_safe_request_aliases() {
    use frona_harness::{Model, ModelRequest};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let mut provider_call = call("tool-0", json!({}));
    provider_call["provider"] = json!({"call_id":"tool-0"});
    provider_call["signature"] = Value::Null;
    provider_call["additional_params"] = Value::Null;
    let mut first = call("first-local", json!({"query":"first"}));
    first["additional_params"] = json!({"format":"anthropic-claude-v1"});
    let mut second = call("second-local", json!({"query":"second"}));
    second["additional_params"] = Value::Null;
    let mut records = vec![
        opened(json!([{"type":"text","text":"look up answers"}])),
        record(
            1,
            json!({"Response":{"response":0,"message":1,"origin":"openrouter/mock-model","offered_tools":[],"output":output(json!([provider_call,first,second]))}}),
        ),
    ];
    for action in 0..3 {
        records.push(record(2+action,json!({"ActionResolved":{"action":{"conversation":"00000000-0000-0000-0000-000000000000","response":0,"action":action},"resolution":{"Result":{"content":[{"type":"text","text":"result"}],"metadata":null}}}})));
    }
    let replay = decode_jsonl(&bytes(&records)).unwrap().replay;
    let archived = encode_jsonl(replay.conversation.history()).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"chat_next","object":"chat.completion","created":0,"model":"mock-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]
    }))).expect(1).mount(&server).await;
    let model = Model::builder("openrouter", "mock-model")
        .api_key("fixture-key")
        .base_url(server.uri())
        .build()
        .unwrap();
    let mut request = ModelRequest::new("unused");
    request.messages = replay.conversation.messages().to_vec();
    // Rig removes this failed turn before allocating aliases. It must not shift
    // either surviving local call's correlation id.
    let mut dropped = AssistantMessage::new(vec![AssistantContent::ToolCall(
        rig_core::message::ToolCall::new(
            rig_core::message::CallId::from_wire(""),
            rig_core::message::ToolFunction::new("lookup".try_into().unwrap(), json!({})),
        ),
    )]);
    dropped.stop = Some(rig_core::message::StopReason::Error(
        "old failed turn".into(),
    ));
    request.messages.insert(1, Message::Assistant(dropped));
    request
        .tools
        .push(rig_core::completion::request::ToolDefinition {
            annotations: Default::default(),
            name: "lookup".try_into().unwrap(),
            description: "Lookup".into(),
            parameters: json!({"type":"object"}),
        });
    model.complete(request).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    let calls = assistant["tool_calls"].as_array().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|call| call["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["tool-0", "tool-1", "tool-2"]
    );
    let details = assistant["reasoning_details"].as_array().unwrap();
    assert_eq!(details[0]["id"], calls[1]["id"]);
    assert_eq!(details[1]["id"], calls[2]["id"]);
    for detail in details {
        assert_eq!(detail["data"], "signature+/=");
    }
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["tool-0", "tool-1", "tool-2"]
    );
    assert!(!body.to_string().contains("frona_legacy_"));
    assert_eq!(
        encode_jsonl(replay.conversation.history()).unwrap(),
        archived
    );
    let Message::Assistant(assistant) = &replay.conversation.messages()[1] else {
        panic!()
    };
    assert!(matches!(&assistant.content[1],AssistantContent::ToolCall(call) if call.id.is_local()));
}

#[tokio::test]
async fn context_bound_openrouter_signed_legacy_fails_before_network() {
    use frona_harness::{Model, ModelRequest};
    let model_id = "anthropic/claude-opus-5.5";
    let records = resolved_call_records(
        &format!("openrouter/{model_id}"),
        call("local-handle", json!({})),
    );
    let replay = decode_jsonl(&bytes(&records)).unwrap().replay;
    let server = wiremock::MockServer::start().await;
    let model = Model::builder("openrouter", model_id)
        .api_key("fixture-key")
        .base_url(server.uri())
        .build()
        .unwrap();
    let mut request = ModelRequest::new("unused");
    request.messages = replay.conversation.messages().to_vec();
    let error = model.complete(request).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("original prompt/tool context fingerprint was not recorded"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
