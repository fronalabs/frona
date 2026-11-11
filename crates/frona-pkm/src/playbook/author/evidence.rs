use super::*;
pub(super) fn evidence_context(
    knowledge: &KnowledgeSnapshot,
    memories: &[&KnowledgeMemory],
) -> Result<
    (
        String,
        Vec<InvocationEvidence>,
        Arc<crate::playbook::tools::TranscriptLookup>,
    ),
    Error,
> {
    let mut anchors = BTreeMap::<String, BTreeSet<String>>::new();
    for memory in memories {
        for evidence in &memory.evidence {
            match &evidence.source {
                EvidenceSource::UserMessage {
                    chat_id,
                    message_id,
                    ..
                }
                | EvidenceSource::AgentMessage {
                    chat_id,
                    message_id,
                    ..
                } => {
                    anchors
                        .entry(chat_id.clone())
                        .or_default()
                        .insert(message_id.clone());
                }
                _ => {}
            }
        }
    }
    let mut transcript = String::new();
    let mut transcript_tokens = 0usize;
    let mut selected_agents = BTreeMap::<String, BTreeSet<String>>::new();
    let mut lookup = crate::playbook::tools::TranscriptLookup::default();
    let mut next_cursor = 1usize;
    for (chat_id, message_ids) in &anchors {
        let Some(conversation) = knowledge.conversations.get(chat_id) else {
            continue;
        };
        let messages = conversation.messages.clone();
        let tool_calls = conversation.invocations.clone();
        for index in 0..messages.len() {
            lookup
                .cursors
                .insert(format!("t{next_cursor}"), (chat_id.clone(), index));
            next_cursor += 1;
        }
        let mut selected = BTreeSet::new();
        for anchor in message_ids {
            let Some(end) = messages.iter().position(|message| &message.id == anchor) else {
                continue;
            };
            let mut start = end;
            let mut agents = 0usize;
            while start > 0 && agents < 10 {
                start -= 1;
                if messages[start].role == MessageRole::Agent {
                    agents += 1;
                }
            }
            selected.extend(start..=end);
        }
        for index in selected {
            let message = &messages[index];
            let local = lookup
                .cursors
                .iter()
                .find_map(|(local, held)| {
                    (held == &(chat_id.clone(), index)).then_some(local.as_str())
                })
                .unwrap_or("?");
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Agent => "agent",
                MessageRole::System => "system",
                _ => "event",
            };
            let line = format!(
                "[{local} {role}] {}\n",
                frona_text::redaction::redact_text(&crate::playbook::transcript::message_text(
                    &message.id,
                    &message.content,
                    &tool_calls,
                ))
            );
            let cost = frona_harness::inference::context::estimate_tokens(&line);
            if transcript_tokens + cost <= 16_000 {
                transcript_tokens += cost;
                transcript.push_str(&line);
            }
            if message.role == MessageRole::Agent {
                selected_agents
                    .entry(chat_id.clone())
                    .or_default()
                    .insert(message.id.clone());
            }
        }
        lookup.tool_calls.insert(chat_id.clone(), tool_calls);
        lookup.chats.insert(chat_id.clone(), messages);
    }
    if transcript.is_empty() {
        transcript.push_str("(source transcript unavailable)");
    }

    let mut out = Vec::new();
    let mut tokens = 0usize;
    for (chat_id, message_ids) in selected_agents {
        for call in lookup.tool_calls.get(&chat_id).into_iter().flatten() {
            if !message_ids.contains(&call.message_id) || out.len() >= 100 {
                continue;
            }
            let reference = invocation_projection(call);
            let Some(reference) = reference else { continue };
            let cost = frona_harness::inference::context::estimate_tokens(&reference.reference);
            if cost > 2_000 || tokens + cost > 16_000 {
                continue;
            }
            tokens += cost;
            out.push(reference);
        }
    }
    Ok((transcript, out, Arc::new(lookup)))
}
fn invocation_projection(call: &crate::playbook::Invocation) -> Option<InvocationEvidence> {
    let name = call.name.to_ascii_lowercase();
    let value = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| {
                call.arguments
                    .get(*key)
                    .and_then(|value| value.as_str())
                    .map(str::trim)
            })
            .filter(|value| !value.is_empty())
    };
    let reference = if name.contains("browser") {
        serde_json::json!({"url": value(&["url"])?}).to_string()
    } else if name.contains("search") {
        serde_json::json!({"query": value(&["query", "q"])?}).to_string()
    } else if name.contains("python") || name == "node" || name.contains("nodejs") {
        serde_json::json!({"code": value(&["code", "script"])?}).to_string()
    } else if name == "shell" || name.contains("exec_command") || name.contains("terminal") {
        serde_json::json!({"command": value(&["command", "cmd"])?}).to_string()
    } else {
        serde_json::json!({
            "tool": call.name,
            "parameters": crate::redaction::redact_json(&call.arguments),
        })
        .to_string()
    };
    Some(InvocationEvidence {
        id: call.id.clone(),
        tool: call.name.clone(),
        reference: frona_text::redaction::redact_text(&reference),
    })
}
