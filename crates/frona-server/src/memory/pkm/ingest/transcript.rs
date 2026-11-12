//! Prepare transcript text and provenance from server messages, tasks, and tool calls.

use crate::chat::message::models::{MessageEvent, MessageResponse, MessageRole};
use crate::contact::service::ContactService;
use crate::core::error::AppError;
use crate::db::repo::tool_calls::ToolCallRepository;
use crate::inference::tool_call::ToolCall;
use crate::memory::pkm::{PkmService, model::KnowledgeShortMemory, vault::VaultScope};
use chrono::{DateTime, Utc};

pub(crate) fn push_message(out: &mut String, handle: &str, speaker: &str, text: &str) {
    out.push_str(&format!("[{handle}] {speaker}: {text}\n"));
}

pub(crate) fn push_agent_message(out: &mut String, handle: &str, text: &str, recall: &str) {
    out.push_str(&format!("[{handle}] Agent: {text}\n"));
    out.push_str(recall);
}

pub(crate) fn push_remembered(out: &mut String, text: &str) {
    out.push_str(&format!("[remembered: {}]\n", text.trim()));
}

pub(crate) fn push_task(out: &mut String, handle: &str, text: &str) {
    out.push_str(&format!("[{handle}] {text}\n"));
}

pub(crate) fn external_note(content: &str) -> String {
    format!("[m1] External note: {content}")
}

pub(crate) fn message_text(message_id: &str, final_text: &str, tool_calls: &[ToolCall]) -> String {
    let mut turns = tool_calls
        .iter()
        .filter(|call| call.message_id == message_id)
        .filter_map(|call| call.turn_text.as_deref().map(|text| (call, text.trim())))
        .filter(|(_, text)| !text.is_empty())
        .collect::<Vec<_>>();
    turns.sort_by(|(left, _), (right, _)| {
        (left.turn, left.created_at, &left.id).cmp(&(right.turn, right.created_at, &right.id))
    });
    turns
        .into_iter()
        .map(|(_, text)| rtb_redact::string(text).into_owned())
        .chain((!final_text.trim().is_empty()).then(|| final_text.trim().to_string()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Strip runtime and provider metadata before handing recorded executions to PKM.
pub(crate) fn tool_execution(call: &ToolCall) -> frona_pkm::evidence::ToolExecution {
    frona_pkm::evidence::ToolExecution {
        id: call.id.clone(),
        chat_id: call.chat_id.clone(),
        message_id: call.message_id.clone(),
        turn: call.turn,
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        result: call.result.clone(),
        success: call.success,
        created_at: call.created_at,
        user_response: call
            .hitl
            .as_ref()
            .is_some_and(|hitl| hitl.status == crate::inference::tool_call::ToolStatus::Resolved),
    }
}

pub(in crate::memory::pkm) struct TranscriptInput<'a> {
    pub(in crate::memory::pkm) messages: &'a [MessageResponse],
    pub(in crate::memory::pkm) short: &'a [KnowledgeShortMemory],
    pub(in crate::memory::pkm) contact_service: &'a ContactService,
    pub(in crate::memory::pkm) task_service: &'a crate::agent::task::service::TaskService,
    pub(in crate::memory::pkm) tool_calls: &'a [crate::inference::tool_call::ToolCall],
    pub(in crate::memory::pkm) agent_message_ids: &'a [String],
    pub(in crate::memory::pkm) assertion_message_ids: &'a [String],
    pub(in crate::memory::pkm) task_evidence:
        &'a std::collections::HashMap<String, Vec<crate::inference::tool_call::ToolCall>>,
    pub(in crate::memory::pkm) user_id: &'a str,
    pub(in crate::memory::pkm) vault: &'a VaultScope,
}

pub(in crate::memory::pkm) fn completed_task_result_links(
    messages: &[MessageResponse],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut ordered = messages.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|message| message.created_at);
    let mut pending = Vec::new();
    let mut links = std::collections::HashMap::<String, Vec<String>>::new();
    for message in ordered {
        if let Some(MessageEvent::TaskCompletion {
            task_id, status, ..
        }) = &message.event
            && *status == crate::agent::task::models::TaskStatus::Completed
        {
            pending.push(task_id.clone());
            continue;
        }
        if message.role == MessageRole::Agent && !pending.is_empty() {
            links
                .entry(message.id.clone())
                .or_default()
                .append(&mut pending);
        }
    }
    links
}

fn task_target_at(task: &crate::agent::task::models::Task) -> Option<DateTime<Utc>> {
    match &task.kind {
        crate::agent::task::models::TaskKind::Cron { next_run_at, .. } => *next_run_at,
        crate::agent::task::models::TaskKind::CronRun { fire_at, .. } => Some(*fire_at),
        _ => task.run_at,
    }
}

fn render_task_lifecycle(
    lifecycle: &str,
    title: &str,
    event_at: DateTime<Utc>,
    target_at: Option<DateTime<Utc>>,
) -> String {
    use chrono::SecondsFormat;

    let event_at = event_at.to_rfc3339_opts(SecondsFormat::Millis, true);
    match target_at {
        Some(target_at) => format!(
            "[task {lifecycle} event_at={} target_at={}] {}",
            event_at,
            target_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            title.trim(),
        ),
        None => format!("[task {lifecycle} event_at={event_at}] {}", title.trim()),
    }
}

pub(in crate::memory::pkm) async fn collect_task_tree_tool_calls(
    root_task_ids: &[String],
    task_service: &crate::agent::task::service::TaskService,
    tool_calls: &crate::db::repo::tool_calls::SurrealToolCallRepo,
) -> Result<Vec<crate::inference::tool_call::ToolCall>, AppError> {
    let mut pending = std::collections::VecDeque::from(root_task_ids.to_vec());
    let mut visited_tasks = std::collections::HashSet::new();
    let mut visited_chats = std::collections::HashSet::new();
    let mut calls = Vec::new();
    while let Some(task_id) = pending.pop_front() {
        if !visited_tasks.insert(task_id.clone()) {
            continue;
        }
        let Some(task) = task_service.find_by_id(&task_id).await? else {
            continue;
        };
        let Some(chat_id) = task.chat_id else {
            continue;
        };
        if visited_chats.insert(chat_id.clone()) {
            calls.extend(tool_calls.find_by_chat_id(&chat_id).await?);
        }
        for child in task_service.find_by_source_chat_id(&chat_id).await? {
            pending.push_back(child.id);
        }
    }
    calls.sort_by(|left, right| {
        (left.created_at, &left.chat_id, left.turn, &left.id).cmp(&(
            right.created_at,
            &right.chat_id,
            right.turn,
            &right.id,
        ))
    });
    calls.dedup_by(|left, right| left.id == right.id);
    Ok(calls)
}

impl PkmService {
    /// Render new messages + un-consolidated short memories into one speaker-labeled
    /// transcript, interleaved by `created_at`: contact names resolved for group
    /// chats, `System` dropped, short memories as `[remembered: …]`.
    pub(in crate::memory::pkm) async fn build_transcript(
        &self,
        input: TranscriptInput<'_>,
    ) -> (
        String,
        Vec<frona_pkm::TemporalSource>,
        Vec<frona_pkm::TranscriptEvidenceSource>,
        frona_pkm::RecallProjection,
    ) {
        let TranscriptInput {
            messages,
            short,
            contact_service,
            task_service,
            tool_calls,
            agent_message_ids,
            assertion_message_ids,
            task_evidence,
            user_id,
            vault,
        } = input;
        let current_message_ids = messages
            .iter()
            .map(|message| message.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let current_tool_calls = tool_calls
            .iter()
            .filter(|call| current_message_ids.contains(call.message_id.as_str()))
            .map(tool_execution)
            .collect::<Vec<_>>();
        let is_memory_path = |value: &str| {
            self.storage.is_user_pkm_path(vault.handle(), value)
                || vault.page_from_any(value).is_some()
                || value.split_whitespace().any(|token| {
                    let token = token.trim_matches(|character: char| {
                        matches!(character, '"' | '\'' | ',' | ')' | ']' | '}')
                    });
                    let token = token.strip_prefix("path=").unwrap_or(token);
                    self.storage.is_user_pkm_path(vault.handle(), token)
                        || vault.page_from_any(token).is_some()
                })
        };
        let executions = tool_calls.iter().map(tool_execution).collect::<Vec<_>>();
        let task_executions = task_evidence
            .iter()
            .map(|(message, calls)| (message.clone(), calls.iter().map(tool_execution).collect()))
            .collect();
        let evidence = frona_pkm::ToolEvidenceProjection::new_with_task_evidence(
            &executions,
            agent_message_ids,
            assertion_message_ids,
            &task_executions,
            self.memory_config
                .pkm_extract_agent_evidence_lookback_messages,
            self.memory_config
                .pkm_extract_agent_evidence_result_token_cap,
            is_memory_path,
        );
        let recall = frona_pkm::RecallProjection::new(&current_tool_calls, is_memory_path)
            .with_evidence(evidence);
        enum Item<'a> {
            Msg(&'a MessageResponse),
            Mem(&'a KnowledgeShortMemory),
            Task(&'a crate::inference::tool_call::ToolCall),
            Hitl(&'a crate::inference::tool_call::ToolCall),
        }
        let mut items: Vec<(DateTime<Utc>, Item)> = Vec::new();
        for m in messages {
            items.push((m.created_at, Item::Msg(m)));
        }
        for s in short {
            items.push((s.created_at, Item::Mem(s)));
        }
        for call in tool_calls {
            if !current_message_ids.contains(call.message_id.as_str()) {
                continue;
            }
            if call.success && matches!(call.name.as_str(), "create_task" | "create_recurring_task")
            {
                items.push((call.created_at, Item::Task(call)));
            }
            if call.hitl.as_ref().is_some_and(|hitl| {
                hitl.status == crate::inference::tool_call::ToolStatus::Resolved
                    && matches!(
                        &hitl.response,
                        Some(
                            crate::inference::hitl::HitlResponse::Choice(_)
                                | crate::inference::hitl::HitlResponse::Approval(_)
                        )
                    )
            }) {
                items.push((call.created_at, Item::Hitl(call)));
            }
        }
        items.sort_by_key(|(t, _)| *t);

        let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut out = String::new();
        let mut temporal_sources = Vec::new();
        let mut evidence_sources = Vec::new();
        let mut next_handle = 1usize;
        for (created_at, item) in items {
            match item {
                Item::Msg(m) => {
                    if m.role == MessageRole::TaskCompletion {
                        let Some(MessageEvent::TaskCompletion {
                            task_id, status, ..
                        }) = &m.event
                        else {
                            continue;
                        };
                        let Ok(Some(task)) = task_service.find_by_id(task_id).await else {
                            continue;
                        };
                        let lifecycle = match status {
                            crate::agent::task::models::TaskStatus::Completed => "completed",
                            crate::agent::task::models::TaskStatus::Failed => "failed",
                            crate::agent::task::models::TaskStatus::Cancelled => "cancelled",
                            _ => continue,
                        };
                        let target_at = task_target_at(&task);
                        let text =
                            render_task_lifecycle(lifecycle, &task.title, created_at, target_at);
                        let handle = format!("m{next_handle}");
                        next_handle += 1;
                        push_task(&mut out, &handle, &text);
                        evidence_sources.push(frona_pkm::TranscriptEvidenceSource {
                            handle: handle.clone(),
                            text: text.clone(),
                            kind: frona_pkm::TranscriptEvidenceKind::TaskLifecycle {
                                message_id: m.id.clone(),
                                chat_id: m.chat_id.clone(),
                                task_id: task_id.clone(),
                            },
                        });
                        temporal_sources.push(frona_pkm::TemporalSource {
                            handle,
                            text,
                            created_at,
                            task_event_at: Some(created_at),
                            task_target_at: target_at,
                        });
                        continue;
                    }
                    let agent_text = if m.role == MessageRole::Agent {
                        Some(message_text(&m.id, &m.content, tool_calls))
                    } else {
                        None
                    };
                    let text = agent_text
                        .as_ref()
                        .cloned()
                        .unwrap_or_else(|| m.content.trim().to_string());
                    if text.is_empty() {
                        continue;
                    }
                    let speaker = match m.role {
                        MessageRole::User => "User".to_string(),
                        MessageRole::Agent => "Agent".to_string(),
                        MessageRole::Contact => match &m.contact_id {
                            Some(cid) => {
                                if let Some(n) = names.get(cid) {
                                    n.clone()
                                } else {
                                    let n = contact_service
                                        .get(user_id, cid)
                                        .await
                                        .map(|c| c.name)
                                        .unwrap_or_else(|_| "Contact".to_string());
                                    names.insert(cid.clone(), n.clone());
                                    n
                                }
                            }
                            None => "Contact".to_string(),
                        },
                        _ => continue,
                    };
                    let handle = format!("m{next_handle}");
                    next_handle += 1;
                    if let Some(agent_text) = &agent_text {
                        push_agent_message(
                            &mut out,
                            &handle,
                            agent_text,
                            &recall.render_for_message(&m.id, &handle),
                        );
                    } else {
                        push_message(&mut out, &handle, &speaker, &text);
                    }
                    let kind = match m.role {
                        MessageRole::User => Some(frona_pkm::TranscriptEvidenceKind::UserMessage {
                            message_id: m.id.clone(),
                            chat_id: m.chat_id.clone(),
                        }),
                        MessageRole::Agent => {
                            Some(frona_pkm::TranscriptEvidenceKind::AgentMessage {
                                message_id: m.id.clone(),
                                agent_id: m.agent_id.clone().unwrap_or_default(),
                                chat_id: m.chat_id.clone(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        evidence_sources.push(frona_pkm::TranscriptEvidenceSource {
                            handle: handle.clone(),
                            text: text.clone(),
                            kind,
                        });
                    }
                    temporal_sources.push(frona_pkm::TemporalSource {
                        handle,
                        text,
                        created_at,
                        task_event_at: None,
                        task_target_at: None,
                    });
                }
                Item::Mem(s) => push_remembered(&mut out, &s.content),
                Item::Task(call) => {
                    let Some(title) = call.arguments.get("title").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    let title = title.trim();
                    if title.is_empty() {
                        continue;
                    }
                    let Some(task_id) = serde_json::from_str::<serde_json::Value>(&call.result)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("task_id")
                                .or_else(|| value.get("id"))
                                .and_then(|id| id.as_str())
                                .map(str::to_string)
                        })
                    else {
                        continue;
                    };
                    let target_at = match task_service.find_by_id(&task_id).await {
                        Ok(task) => task.as_ref().and_then(task_target_at),
                        Err(error) => {
                            tracing::warn!(%task_id, %error, "pkm extract could not load scheduled task time");
                            None
                        }
                    };
                    let text = render_task_lifecycle("scheduled", title, created_at, target_at);
                    let handle = format!("m{next_handle}");
                    next_handle += 1;
                    push_task(&mut out, &handle, &text);
                    evidence_sources.push(frona_pkm::TranscriptEvidenceSource {
                        handle: handle.clone(),
                        text: text.clone(),
                        kind: frona_pkm::TranscriptEvidenceKind::TaskLifecycle {
                            message_id: call.message_id.clone(),
                            chat_id: call.chat_id.clone(),
                            task_id,
                        },
                    });
                    temporal_sources.push(frona_pkm::TemporalSource {
                        handle,
                        text,
                        created_at,
                        task_event_at: Some(created_at),
                        task_target_at: target_at,
                    });
                }
                Item::Hitl(call) => {
                    let Some(hitl) = &call.hitl else { continue };
                    let text = match &hitl.response {
                        Some(crate::inference::hitl::HitlResponse::Choice(value)) => value.clone(),
                        Some(crate::inference::hitl::HitlResponse::Approval(value)) => {
                            if *value {
                                "approved".to_string()
                            } else {
                                "denied".to_string()
                            }
                        }
                        _ => continue,
                    };
                    let handle = format!("m{next_handle}");
                    next_handle += 1;
                    push_message(&mut out, &handle, "User", text.trim());
                    evidence_sources.push(frona_pkm::TranscriptEvidenceSource {
                        handle: handle.clone(),
                        text: text.clone(),
                        kind: frona_pkm::TranscriptEvidenceKind::UserMessage {
                            message_id: call.message_id.clone(),
                            chat_id: call.chat_id.clone(),
                        },
                    });
                    temporal_sources.push(frona_pkm::TemporalSource {
                        handle,
                        text,
                        created_at,
                        task_event_at: None,
                        task_target_at: None,
                    });
                }
            }
        }
        (out, temporal_sources, evidence_sources, recall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::task::models::{Task, TaskKind, TaskStatus};
    use crate::chat::broadcast::BroadcastService;
    use crate::chat::message::models::MessageStatus;
    use crate::core::repository::Repository;
    use crate::db::repo::generic::SurrealRepo;
    use crate::inference::tool_call::ToolCall;
    use chrono::TimeZone;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Mem;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn msg(created: DateTime<Utc>, status: Option<MessageStatus>) -> MessageResponse {
        MessageResponse {
            id: String::new(),
            chat_id: String::new(),
            role: MessageRole::Agent,
            content: "x".into(),
            agent_id: None,
            event: None,
            attachments: Vec::new(),
            contact_id: None,
            status,
            error: None,
            reasoning: None,
            from_address: None,
            delivery: None,
            tool_calls: Vec::new(),
            command: None,
            metadata: Default::default(),
            created_at: created,
        }
    }

    #[test]
    fn completed_tasks_attach_to_the_next_agent_result_message() {
        let mut first = msg(at(100), Some(MessageStatus::Completed));
        first.id = "task-completion-1".into();
        first.role = MessageRole::TaskCompletion;
        first.event = Some(MessageEvent::TaskCompletion {
            task_id: "task-1".into(),
            chat_id: Some("task-chat-1".into()),
            status: crate::agent::task::models::TaskStatus::Completed,
            summary: None,
            schema: None,
        });
        let mut second = msg(at(110), Some(MessageStatus::Completed));
        second.id = "task-completion-2".into();
        second.role = MessageRole::TaskCompletion;
        second.event = Some(MessageEvent::TaskCompletion {
            task_id: "task-2".into(),
            chat_id: Some("task-chat-2".into()),
            status: crate::agent::task::models::TaskStatus::Completed,
            summary: None,
            schema: None,
        });
        let mut result = msg(at(120), Some(MessageStatus::Completed));
        result.id = "parent-result".into();
        let mut later = msg(at(130), Some(MessageStatus::Completed));
        later.id = "later-agent".into();

        let links = completed_task_result_links(&[first, second, result, later]);

        assert_eq!(
            links.get("parent-result"),
            Some(&vec!["task-1".into(), "task-2".into()])
        );
        assert!(!links.contains_key("later-agent"));
    }

    fn stored_task(id: &str, chat_id: &str, source_chat_id: Option<&str>) -> Task {
        Task {
            id: id.into(),
            user_id: "user".into(),
            agent_id: "agent".into(),
            space_id: None,
            chat_id: Some(chat_id.into()),
            title: id.into(),
            description: String::new(),
            status: TaskStatus::Completed,
            kind: TaskKind::Direct {
                source_chat_id: source_chat_id.map(str::to_string),
            },
            run_at: None,
            result_summary: None,
            error_message: None,
            quarantined: false,
            result_schema: None,
            result_description: None,
            created_at: at(1),
            updated_at: at(1),
        }
    }

    #[test]
    fn task_lifecycle_text_contains_event_and_target_times() {
        let text = render_task_lifecycle("scheduled", "Review report", at(100), Some(at(200)));

        assert_eq!(
            text,
            "[task scheduled event_at=1970-01-01T00:01:40.000Z target_at=1970-01-01T00:03:20.000Z] Review report"
        );
    }

    #[test]
    fn task_target_uses_run_time_and_recurring_fire_time() {
        let mut deferred = stored_task("deferred", "chat", Some("source"));
        deferred.run_at = Some(at(200));
        assert_eq!(task_target_at(&deferred), Some(at(200)));

        let mut recurring_run = stored_task("run", "chat", Some("source"));
        recurring_run.kind = TaskKind::CronRun {
            source_cron_id: "template".into(),
            source_chat_id: Some("source".into()),
            source_agent_id: Some("agent".into()),
            fire_at: at(300),
            sequence_num: 1,
        };
        assert_eq!(task_target_at(&recurring_run), Some(at(300)));
    }

    fn stored_call(id: &str, chat_id: &str, turn: u32) -> ToolCall {
        ToolCall {
            id: id.into(),
            chat_id: chat_id.into(),
            message_id: format!("message-{id}"),
            turn,
            provider_item_id: None,
            provider_call_id: format!("provider-{id}"),
            name: "web_fetch".into(),
            arguments: serde_json::json!({"url":format!("https://example.test/{id}")}),
            result: format!("result {id}"),
            success: true,
            duration_ms: 1,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            signature: None,
            turn_reasoning: None,
            created_at: at(turn as i64),
        }
    }

    #[tokio::test]
    async fn task_tree_collection_reads_direct_and_nested_task_chats() {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        let task_repo = SurrealRepo::<Task>::new(db.clone());
        let tool_repo = SurrealRepo::<ToolCall>::new(db);
        task_repo
            .create(&stored_task("root", "root-chat", None))
            .await
            .unwrap();
        task_repo
            .create(&stored_task("child", "child-chat", Some("root-chat")))
            .await
            .unwrap();
        task_repo
            .create(&stored_task(
                "grandchild",
                "grandchild-chat",
                Some("child-chat"),
            ))
            .await
            .unwrap();
        task_repo
            .create(&stored_task("other", "other-chat", None))
            .await
            .unwrap();
        tool_repo
            .create(&stored_call("root-call", "root-chat", 1))
            .await
            .unwrap();
        tool_repo
            .create(&stored_call("child-call", "child-chat", 2))
            .await
            .unwrap();
        tool_repo
            .create(&stored_call("grandchild-call", "grandchild-chat", 3))
            .await
            .unwrap();
        tool_repo
            .create(&stored_call("other-call", "other-chat", 4))
            .await
            .unwrap();
        let service =
            crate::agent::task::service::TaskService::new(task_repo, BroadcastService::new());

        let calls = collect_task_tree_tool_calls(&["root".into()], &service, &tool_repo)
            .await
            .unwrap();

        assert_eq!(
            calls
                .iter()
                .map(|call| call.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root-call", "child-call", "grandchild-call",]
        );
    }

    fn call(id: &str, message_id: &str, turn: u32, turn_text: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.into(),
            chat_id: "chat-1".into(),
            message_id: message_id.into(),
            turn,
            provider_item_id: None,
            provider_call_id: format!("provider-{id}"),
            name: "web_search".into(),
            arguments: serde_json::json!({"query":"firmware"}),
            result: "result".into(),
            success: true,
            duration_ms: 1,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: turn_text.map(str::to_string),
            signature: Some("opaque-provider-signature-not-evidence".into()),
            turn_reasoning: None,
            created_at: Utc.timestamp_opt(turn as i64, 0).unwrap(),
        }
    }

    #[test]
    fn includes_each_non_empty_tool_turn_before_the_final_response() {
        let calls = vec![
            call("later", "message-1", 2, Some("  second research update  ")),
            call("empty", "message-1", 3, Some("  \n ")),
            call("other", "message-2", 0, Some("unrelated text")),
            call("earlier", "message-1", 1, Some("first research update")),
        ];

        let text = message_text("message-1", "  short final response  ", &calls);

        assert_eq!(
            text,
            concat!(
                "first research update\n\n",
                "second research update\n\n",
                "short final response",
            )
        );
    }

    #[test]
    fn keeps_the_existing_text_shape_when_there_is_no_tool_turn_text() {
        let calls = vec![call("empty", "message-1", 0, None)];

        assert_eq!(
            message_text("message-1", "  final response  ", &calls),
            "final response"
        );
    }

    #[test]
    fn keeps_tool_turn_text_when_the_final_response_is_empty() {
        let calls = vec![call(
            "research",
            "message-1",
            0,
            Some("durable research result"),
        )];

        assert_eq!(
            message_text("message-1", "  ", &calls),
            "durable research result"
        );
    }

    #[test]
    fn transcript_keeps_existing_message_shape() {
        let calls = vec![call("research", "message-1", 0, Some("research update"))];
        let text = message_text("message-1", "final response", &calls);
        let mut rendered = String::new();

        push_agent_message(&mut rendered, "m2", &text, "Recall calls for m2:\n");

        assert_eq!(
            rendered,
            concat!(
                "[m2] Agent: research update\n\nfinal response\n",
                "Recall calls for m2:\n",
            )
        );
    }

    #[test]
    fn transcript_renders_other_items() {
        let mut rendered = String::new();

        push_message(&mut rendered, "m1", "User", "hello");
        push_remembered(&mut rendered, "known fact");
        push_task(&mut rendered, "m2", "Task completed: Check firmware");

        assert_eq!(
            rendered,
            concat!(
                "[m1] User: hello\n",
                "[remembered: known fact]\n",
                "[m2] Task completed: Check firmware\n",
            )
        );
    }

    #[test]
    fn external_note_uses_transcript_format() {
        assert_eq!(
            external_note("firmware note"),
            "[m1] External note: firmware note"
        );
    }
}
