//! Read the Rig 0.42 message shapes stored by earlier Frona versions.
//! Conversion is confined to message fields; tool arguments and checkpoint
//! payloads are application data and must never be recursively rewritten.
use rig_core::message::{AssistantContent, CallId, Origin, Reasoning, ToolCall, ToolFunction};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct OldProviderId {
    call_id: String,
    item_id: Option<String>,
}

#[derive(Deserialize)]
struct OldFunction {
    name: String,
    arguments: Value,
}

#[derive(Deserialize)]
struct OldCall {
    id: String,
    provider: Option<OldProviderId>,
    function: OldFunction,
    signature: Option<String>,
    additional_params: Option<Value>,
}

fn invalid(message: &str) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}

fn local_id(id: &str) -> Result<CallId, serde_json::Error> {
    if id.is_empty() {
        return Err(invalid("empty legacy tool call id"));
    }
    // UUIDv5 preserves id-less call/result correlation across records and opens
    // without asserting that a locally generated handle came from the provider.
    let uuid = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("frona:rig-0.42:call:{id}").as_bytes(),
    );
    serde_json::from_value(json!({"local": uuid}))
}

/// Convert a recorded legacy call without losing signatures or provider item IDs.
pub fn legacy_call(value: Value) -> Result<ToolCall, serde_json::Error> {
    let old: OldCall = serde_json::from_value(value)?;
    let id = match &old.provider {
        Some(provider) if !provider.call_id.is_empty() => CallId::from_wire(&provider.call_id),
        Some(_) => return Err(invalid("empty legacy provider call id")),
        None => local_id(&old.id)?,
    };
    let name = old
        .function
        .name
        .try_into()
        .map_err(|_| invalid("empty legacy tool name"))?;
    let call = ToolCall::new(id, ToolFunction::new(name, old.function.arguments));
    let mut native = if let Some(signature) = old.signature {
        let mut item = json!({"functionCall": {"name":call.function.name, "args":call.function.arguments}, "thoughtSignature":signature});
        if let Some(provider) = &old.provider {
            item["functionCall"]["id"] = json!(provider.call_id);
        }
        Some(item)
    } else if let Some(provider) = &old.provider
        && let Some(item_id) = &provider.item_id
    {
        Some(
            json!({"type":"function_call", "id":item_id, "call_id":provider.call_id,
            "name":call.function.name, "arguments":serde_json::to_string(&call.function.arguments)?}),
        )
    } else {
        None
    };
    if let Some(params) = old.additional_params {
        let item = native.get_or_insert_with(|| json!({}));
        item["frona_legacy_additional_params"] = params;
    }
    if let Some(item) = &mut native {
        item["frona_legacy_call_id"] = json!(old.id);
    }
    match native {
        Some(native) => match AssistantContent::ToolCall(call).with_native(native) {
            AssistantContent::ToolCall(call) => Ok(call),
            _ => unreachable!(),
        },
        None => Ok(call),
    }
}

#[derive(Deserialize)]
struct OldReasoning {
    id: Option<String>,
    content: Vec<OldReasoningContent>,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
enum OldReasoningContent {
    Text {
        text: String,
        signature: Option<String>,
    },
    Encrypted(String),
    Redacted {
        data: String,
    },
    Summary(String),
}

/// Read either current reasoning or the previously persisted ordered blocks.
pub fn reasoning_blocks(value: Value) -> Result<Vec<AssistantContent>, serde_json::Error> {
    if value.get("text").is_some() {
        return serde_json::from_value(value).map(|r| vec![AssistantContent::Reasoning(r)]);
    }
    let old: OldReasoning = serde_json::from_value(value.clone())?;
    let responses = old.content.iter().any(|c| {
        matches!(
            c,
            OldReasoningContent::Encrypted(_) | OldReasoningContent::Summary(_)
        )
    });
    if responses {
        let mut summary = Vec::new();
        let mut encrypted = None;
        for item in old.content {
            match item {
                OldReasoningContent::Summary(text)
                | OldReasoningContent::Text {
                    text,
                    signature: None,
                } => summary.push(json!({"type":"summary_text","text":text})),
                OldReasoningContent::Encrypted(value) if encrypted.is_none() => {
                    encrypted = Some(value)
                }
                _ => {
                    // This is valid legacy OpenRouter data. Keep it intact until
                    // the configured provider determines the wire representation.
                    let text = value["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|part| {
                            part["content"]
                                .as_str()
                                .filter(|_| part["type"] == "summary")
                                .or_else(|| part["content"]["text"].as_str())
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Ok(vec![
                        AssistantContent::Reasoning(Reasoning::new(text))
                            .with_native(json!({"frona_legacy_reasoning":value})),
                    ]);
                }
            }
        }
        let text = summary
            .iter()
            .filter_map(|s| s["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut item = json!({"type":"reasoning", "summary":summary});
        if let Some(id) = old.id {
            item["id"] = json!(id);
        }
        if let Some(value) = encrypted {
            item["encrypted_content"] = json!(value);
        }
        item["frona_legacy_reasoning"] = value;
        return Ok(vec![
            AssistantContent::Reasoning(Reasoning::new(text)).with_native(item),
        ]);
    }
    old.content
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let block = match item {
                OldReasoningContent::Text { text, signature } => {
                    let block = AssistantContent::Reasoning(Reasoning::new(&text));
                    match signature {
                        Some(signature) => block.with_native(
                            json!({"type":"thinking", "thinking":text, "signature":signature}),
                        ),
                        None => block,
                    }
                }
                OldReasoningContent::Redacted { data } => AssistantContent::Reasoning(Reasoning {
                    redacted: true,
                    ..Default::default()
                })
                .with_native(json!({"type":"redacted_thinking", "data":data})),
                _ => return Err(invalid("invalid legacy reasoning block")),
            };
            let mut native = block.native_item().cloned().unwrap_or_else(|| json!({}));
            native["frona_legacy_reasoning"] =
                json!({"id":value["id"],"content":[value["content"][index].clone()]});
            Ok(block.with_native(native))
        })
        .collect()
}

/// Infer only wire identity evidenced by a legacy native item. The caller owns
/// the configured model identity; no signature is fabricated or discarded.
pub fn legacy_origin(content: &[AssistantContent], model: &str) -> Option<Origin> {
    for block in content {
        let Some(item) = block.native_item() else {
            continue;
        };
        if item.get("reasoning_details").is_some() {
            return Some(Origin::new("openai.chat", "openrouter", model));
        }
        if item.get("reasoningContent").is_some() {
            return Some(Origin::new("bedrock.converse", "aws_bedrock", model));
        }
        if item.get("functionCall").is_some() || item.get("thoughtSignature").is_some() {
            return Some(Origin::new("gemini.generate_content", "gcp.gemini", model));
        }
        match item.get("type").and_then(Value::as_str) {
            Some("text") if item.get("citations").is_some() => {
                return Some(Origin::new("anthropic.messages", "anthropic", model));
            }
            Some("message") if item.get("content").is_some() => {
                return Some(Origin::new("openai.responses", "openai", model));
            }
            Some("server_tool_use" | "web_search_tool_result" | "code_execution_tool_result") => {
                return Some(Origin::new("anthropic.messages", "anthropic", model));
            }
            Some("thinking" | "redacted_thinking") => {
                return Some(Origin::new("anthropic.messages", "anthropic", model));
            }
            Some("reasoning" | "function_call") => {
                return Some(Origin::new("openai.responses", "openai", model));
            }
            _ => {}
        }
    }
    None
}

/// Retain legacy provenance while adapting the old shared signature field to
/// the configured wire. Dispatch resolves the actual provider descriptor.
pub fn contextualize_legacy(
    assistant: &mut rig_core::message::AssistantMessage,
    model: &super::provider::ModelConfig,
) {
    use crate::config::ProviderModel;
    if let Some(origin) = &assistant.origin
        && (origin.api.as_str() != "frona.legacy"
            || origin.provider != model.provider_handle.as_str()
            || origin.model != model.model_id)
    {
        return;
    }
    if matches!(model.provider, ProviderModel::OpenRouter { .. }) {
        contextualize_openrouter(assistant);
    }
    for block in &mut assistant.content {
        let Some(item) = block.native_item().cloned() else {
            continue;
        };
        let native = match (&model.provider, item.get("type").and_then(Value::as_str)) {
            (ProviderModel::Gemini { .. }, Some("thinking")) => Some(json!({
                "thought":true, "text":item["thinking"], "thoughtSignature":item["signature"]
            })),
            (ProviderModel::Bedrock { .. }, Some("thinking")) => Some(json!({
                "reasoningContent":{"reasoningText":{"text":item["thinking"],"signature":item["signature"]}}
            })),
            (ProviderModel::Bedrock { .. }, Some("redacted_thinking")) => Some(json!({
                "reasoningContent":{"redactedContent":item["data"]}
            })),
            _ => None,
        };
        if let Some(native) = native {
            *block = block.clone().with_native(native);
        }
    }
    if assistant.origin.is_none()
        && assistant
            .content
            .iter()
            .any(|item| item.native_item().is_some())
    {
        assistant.origin = Some(Origin::new(
            "frona.legacy",
            model.provider_handle.as_str(),
            &model.model_id,
        ));
    }
}

// OpenRouter used the shared signature slots for reasoning_details, including
// metadata attached to a tool call. Restore that separate reasoning channel.
fn contextualize_openrouter(assistant: &mut rig_core::message::AssistantMessage) {
    let mut content = Vec::new();
    let mut detail_count = 0;
    for mut block in std::mem::take(&mut assistant.content) {
        let Some(item) = block.native_item().cloned() else {
            content.push(block);
            continue;
        };
        if let AssistantContent::ToolCall(call) = &mut block {
            let params = &item["frona_legacy_additional_params"];
            let detail = if matches!(
                params["type"].as_str(),
                Some("reasoning.encrypted" | "reasoning.text" | "reasoning.summary")
            ) {
                Some(params.clone())
            } else {
                item.get("thoughtSignature").map(|signature| {
                    let id = params.get("id").filter(|id| !id.is_null()).cloned()
                        .unwrap_or_else(|| {
                            if call.id.is_local() { item["frona_legacy_call_id"].clone() }
                            else { json!(call.id.wire()) }
                        });
                    json!({"type":"reasoning.encrypted", "id":id, "format":params["format"], "index":null, "data":signature})
                })
            };
            if let Some(detail) = detail {
                let mut native = json!({"reasoning_details":[detail]});
                // Only a synthesized correlation id follows the tool's request
                // alias. An explicit provider reasoning id remains byte-for-byte.
                if call.id.is_local()
                    && !matches!(
                        params["type"].as_str(),
                        Some("reasoning.encrypted" | "reasoning.text" | "reasoning.summary")
                    )
                    && params.get("id").is_none_or(Value::is_null)
                {
                    native["frona_legacy_openrouter_call"] = json!(call.id);
                }
                content.push(AssistantContent::Reasoning(Reasoning::default()).with_native(native));
                detail_count += 1;
                call.native = None;
            }
        } else if let Some(old) = item.get("frona_legacy_reasoning") {
            let mut details = Vec::new();
            let id = old["id"].as_str().filter(|id| !id.is_empty());
            if let Some(parts) = old["content"].as_array() {
                for part in parts {
                    let mut detail = json!({"id":id,"format":null,"index":detail_count});
                    match part["type"].as_str() {
                        Some("text") => {
                            detail["type"] = json!("reasoning.text");
                            detail["text"] = part["content"]["text"].clone();
                            detail["signature"] = part["content"]["signature"].clone();
                        }
                        Some("summary") => {
                            detail["type"] = json!("reasoning.summary");
                            detail["summary"] = part["content"].clone();
                        }
                        Some("encrypted" | "redacted") => {
                            detail["type"] = json!("reasoning.encrypted");
                            detail["data"] = if part["type"] == "redacted" {
                                part["content"]["data"].clone()
                            } else {
                                part["content"].clone()
                            };
                        }
                        _ => continue,
                    }
                    details.push(detail);
                    detail_count += 1;
                }
            }
            block = block.with_native(json!({"reasoning_details":details}));
        } else if let Some(details) = item["reasoning_details"].as_array() {
            detail_count += details.len();
        }
        content.push(block);
    }
    assistant.content = content;
}

pub(crate) fn prepare_legacy_history(
    messages: &mut [rig_core::message::Message],
    config: &super::provider::ModelConfig,
    model: &rig_core::DynModel<rig_core::operation::Completion>,
) -> Result<(), super::InferenceError> {
    for message in messages.iter_mut() {
        let rig_core::message::Message::Assistant(assistant) = message else {
            continue;
        };
        contextualize_legacy(assistant, config);
        let Some(origin) = &assistant.origin else {
            continue;
        };
        if origin.api.as_str() != "frona.legacy"
            || origin.provider != config.provider_handle.as_str()
            || origin.model != config.model_id
        {
            continue;
        }
        if assistant
            .content
            .iter()
            .filter_map(AssistantContent::native_item)
            .any(|item| {
                item.get("frona_legacy_reasoning").is_some()
                    && item.as_object().is_some_and(|fields| fields.len() == 1)
                    && item["frona_legacy_reasoning"]["content"]
                        .as_array()
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part["type"] == "encrypted"
                                    || part["content"]["signature"].as_str().is_some()
                            })
                        })
            })
        {
            return Err(super::InferenceError::ConfigError("Cannot replay this legacy reasoning combination outside its original OpenRouter provider".into()));
        }
        let signed = assistant
            .content
            .iter()
            .filter_map(AssistantContent::native_item)
            .any(|item| {
                item.get("signature").is_some()
                    || item.get("data").is_some()
                    || item.get("reasoningContent").is_some()
                    || item["reasoning_details"].as_array().is_some_and(|details| {
                        details.iter().any(|detail| {
                            detail["signature"].as_str().is_some()
                                || (detail["type"] == "reasoning.encrypted"
                                    && detail["data"].as_str().is_some())
                        })
                    })
            });
        if signed && model.spec().is_some_and(|spec| spec.compat.binds_context) {
            return Err(super::InferenceError::ConfigError(
                "Cannot replay legacy signed reasoning for this model: its original prompt/tool context fingerprint was not recorded".into()
            ));
        }
        if let Some(mut resolved) = legacy_origin(&assistant.content, &config.model_id) {
            resolved.provider = model.name().to_owned();
            assistant.origin = Some(resolved);
        }
        for block in &mut assistant.content {
            strip_archival_extras(block);
        }
    }
    if matches!(
        config.provider,
        crate::config::ProviderModel::OpenRouter { .. }
    ) {
        resolve_openrouter_call_ids(messages, &config.model_id, model.spec())?;
    }
    Ok(())
}

// Use Rig's allocator so local correlation follows its ordering and collision
// rules without turning a local identity into a provider-issued one. This wire
// exists only to obtain id spellings; it is never used for transport or auth.
fn resolve_openrouter_call_ids(
    messages: &mut [rig_core::message::Message],
    model: &str,
    spec: Option<&rig_core::catalog::ModelSpec>,
) -> Result<(), super::InferenceError> {
    use rig_core::providers::{
        internal::wire_ids::WireIds,
        openai::{
            OpenAIConfig,
            wire::{Chat, OPENROUTER},
        },
    };
    let mut target = Chat::new(OpenAIConfig::with_key(&OPENROUTER, ""), model);
    if let Some(spec) = spec {
        target = target.with_facts(rig_core::catalog::ModelFacts::new(spec.clone()));
    }
    // Allocate after the same history adaptation that the encoder applies:
    // failed turns may disappear and foreign provider IDs may be normalized.
    // Context-bound signed replay has already failed above. Without declared
    // tools the actual encoder removes calls rather than sending these aliases.
    let adapted = rig_core::completion::history::adapt(messages, &target);
    let ids = WireIds::for_target(&adapted, &target, model);
    let mut resolved = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let rig_core::message::Message::Assistant(assistant) = message else {
            continue;
        };
        for (block_index, block) in assistant.content.iter().enumerate() {
            let Some(item) = block.native_item() else {
                continue;
            };
            let Some(id) = item.get("frona_legacy_openrouter_call") else {
                continue;
            };
            let call: CallId = serde_json::from_value(id.clone()).map_err(|_| {
                super::InferenceError::ConfigError(
                    "Invalid legacy OpenRouter tool correlation".into(),
                )
            })?;
            let alias = ids.of(&call).ok_or_else(|| {
                super::InferenceError::ConfigError(
                    "Legacy OpenRouter reasoning references a missing tool call".into(),
                )
            })?;
            let mut item = item.clone();
            item["reasoning_details"][0]["id"] = json!(alias);
            if let Some(fields) = item.as_object_mut() {
                fields.remove("frona_legacy_openrouter_call");
            }
            resolved.push((message_index, block_index, item));
        }
    }
    for (message_index, block_index, item) in resolved {
        let rig_core::message::Message::Assistant(assistant) = &mut messages[message_index] else {
            unreachable!()
        };
        let block = &mut assistant.content[block_index];
        *block = block.clone().with_native(item);
    }
    Ok(())
}

/// Projection only: legacy extras are archival data, not a provider field.
/// Only a current native item may be rebuilt; edited native is left stale so
/// Rig's normal canonical reconstruction continues to protect its baseline.
fn strip_archival_extras(block: &mut AssistantContent) {
    let Some(mut item) = block.native_item().cloned() else {
        return;
    };
    let Some(fields) = item.as_object_mut() else {
        return;
    };
    let mut removed = false;
    for key in [
        "frona_legacy_additional_params",
        "frona_legacy_call_id",
        "frona_legacy_reasoning",
    ] {
        removed |= fields.remove(key).is_some();
    }
    if !removed {
        return;
    }
    if fields.is_empty() {
        match block {
            AssistantContent::Text(text) => text.native = None,
            AssistantContent::ToolCall(call) => call.native = None,
            AssistantContent::Reasoning(reasoning) => reasoning.native = None,
            AssistantContent::Image(image) => image.native = None,
            _ => {}
        }
    } else {
        *block = block.clone().with_native(item);
    }
}

pub(crate) fn upgrade_record(value: &mut Value) -> Result<(), serde_json::Error> {
    let event = value
        .get_mut("event")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("missing history event"))?;
    for (kind, event) in event {
        match kind.as_str() {
            "Opened" => {
                if let Some(items) = event.get_mut("initial").and_then(Value::as_array_mut) {
                    users(items)?;
                }
            }
            "User" => {
                if let Some(items) = event.get_mut("content").and_then(Value::as_array_mut) {
                    users(items)?;
                }
            }
            "Response" | "Compacted" => {
                let origin = event
                    .get(if kind == "Compacted" {
                        "summarizer"
                    } else {
                        "origin"
                    })
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(output) = event.get_mut("output") {
                    output_content(output, origin.as_deref())?;
                }
                if let Some(entries) = event
                    .pointer_mut("/view/entries")
                    .and_then(Value::as_array_mut)
                {
                    for entry in entries {
                        if let Some(message) = entry.pointer_mut("/Summary/message")
                            && message.get("role").and_then(Value::as_str) == Some("user")
                            && let Some(items) =
                                message.get_mut("content").and_then(Value::as_array_mut)
                        {
                            users(items)?;
                        }
                    }
                }
            }
            "ActionResolved" => {
                if let Some(resolution) = event.get_mut("resolution").and_then(Value::as_object_mut)
                {
                    for output in resolution.values_mut() {
                        tool_output(output)?;
                    }
                }
            }
            "SubmissionCompleted" => {
                if let Some(output) = event.get_mut("acknowledgement") {
                    tool_output(output)?;
                }
            }
            _ => {}
        }
    }
    value["version"] = json!(2);
    Ok(())
}

fn output_content(output: &mut Value, source: Option<&str>) -> Result<(), serde_json::Error> {
    if let Some(items) = output.get_mut("content").and_then(Value::as_array_mut) {
        let mut converted = Vec::new();
        for item in std::mem::take(items) {
            match item.get("type").and_then(Value::as_str) {
                Some("toolcall") => converted.push(AssistantContent::ToolCall(legacy_call(item)?)),
                Some("reasoning") => converted.extend(reasoning_blocks(item)?),
                _ => converted.push(serde_json::from_value(media_content(item)?)?),
            }
        }
        *items = converted
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()?;
        if let Some((handle, model)) = source.and_then(|source| source.split_once('/')) {
            output["origin"] = serde_json::to_value(Origin::new("frona.legacy", handle, model))?;
        }
    }
    Ok(())
}

fn media_content(mut item: Value) -> Result<Value, serde_json::Error> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if !matches!(kind.as_str(), "text" | "image") {
        return Ok(item);
    }
    let Some(params) = item
        .get_mut("additional_params")
        .map(Value::take)
        .filter(|v| !v.is_null())
    else {
        return Ok(item);
    };
    // Text/image extras moved to native blocks. Recognized native envelopes are
    // restored exactly; other extras remain available without claiming a wire.
    let native = if let Some(raw) = params.get("anthropic_content") {
        raw.clone()
    } else if let Some(extras) = params.get("openai_responses").and_then(Value::as_object) {
        let mut raw = extras.clone();
        raw.insert("type".into(), json!("output_text"));
        raw.insert("text".into(), item["text"].clone());
        json!({"type":"message","role":"assistant","content":[Value::Object(raw)]})
    } else if params.get("citations").is_some() && kind == "text" {
        let mut raw = params
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("invalid legacy text extras"))?;
        raw.insert("type".into(), json!("text"));
        raw.insert("text".into(), item["text"].clone());
        Value::Object(raw)
    } else {
        json!({"frona_legacy_additional_params":params})
    };
    let block: AssistantContent = serde_json::from_value(item)?;
    serde_json::to_value(block.with_native(native))
}

fn tool_output(output: &mut Value) -> Result<(), serde_json::Error> {
    if let Some(content) = output.get_mut("content").and_then(Value::as_array_mut) {
        for item in content {
            *item = media_content(std::mem::take(item))?;
        }
    }
    Ok(())
}

fn users(items: &mut [Value]) -> Result<(), serde_json::Error> {
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("toolresult") {
            let provider = item
                .get("provider")
                .cloned()
                .filter(|v| !v.is_null())
                .map(serde_json::from_value::<OldProviderId>)
                .transpose()?;
            let old = item
                .get("call")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("missing legacy result id"))?;
            let id = match provider {
                Some(provider) if !provider.call_id.is_empty() => {
                    CallId::from_wire(provider.call_id)
                }
                Some(_) => return Err(invalid("empty legacy provider call id")),
                None => local_id(old)?,
            };
            item["call"] = serde_json::to_value(id)?;
            tool_output(item)?;
        } else {
            *item = media_content(std::mem::take(item))?;
        }
    }
    Ok(())
}
