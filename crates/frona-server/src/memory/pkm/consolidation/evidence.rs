use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use frona_derive::agent_tool;
use frona_text::GroundingText;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::agent::prompt::PromptLoader;
use crate::core::error::AppError;
use crate::inference::tool_call::ToolCall;
use crate::tool::{InferenceContext, ToolOutput, str_arg};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionEvidenceClass {
    Recall,
    MemoryMutation,
    UserResponse,
    TaskControl,
    Evidence,
    Sensitive,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ToolSupportCitation {
    WebSearch {
        execution: String,
        url: Option<String>,
    },
    WebPage {
        execution: String,
        url: Option<String>,
    },
    ToolResult {
        execution: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectedEvidenceCall {
    pub local_id: String,
    pub tool_call_id: String,
    pub chat_id: String,
    pub message_id: String,
    pub name: String,
    pub arguments: Value,
    pub result: String,
    pub(crate) query: Option<String>,
    pub(crate) url: Option<String>,
    pub class: ExecutionEvidenceClass,
    chunks: Vec<String>,
}

impl ProjectedEvidenceCall {
    pub(crate) fn searchable_text(&self) -> String {
        format!(
            "{}\n{}",
            serde_json::to_string(&self.arguments).unwrap_or_default(),
            self.result
        )
    }

    pub(crate) fn critical_value_text(&self) -> String {
        format!("{}\n{}", self.name, self.searchable_text())
    }

    pub(crate) fn support_citation(&self) -> ToolSupportCitation {
        let lower = self.name.to_ascii_lowercase();
        if lower == "web_search" || (lower.contains("search") && self.url.is_some()) {
            ToolSupportCitation::WebSearch {
                execution: self.local_id.clone(),
                url: self.url.clone(),
            }
        } else if (lower.contains("fetch")
            || lower.contains("browser")
            || lower == "web"
            || curl_requested_url(&self.arguments).is_some())
            && self.url.is_some()
        {
            ToolSupportCitation::WebPage {
                execution: self.local_id.clone(),
                url: self.url.clone(),
            }
        } else {
            ToolSupportCitation::ToolResult {
                execution: self.local_id.clone(),
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolEvidenceProjection {
    by_assertion_message: BTreeMap<String, Vec<String>>,
    by_local_id: HashMap<String, ProjectedEvidenceCall>,
    handles: Arc<Mutex<EvidenceHandleState>>,
    pub(crate) result_token_cap: usize,
}

#[derive(Debug, Clone)]
struct EvidenceHandleTarget {
    call_local_id: String,
    chunk_index: usize,
}

#[derive(Debug, Default)]
struct EvidenceHandleState {
    next_by_prompt_message: HashMap<String, usize>,
    by_id: HashMap<String, EvidenceHandleTarget>,
    by_target: HashMap<(String, usize), String>,
}

pub(crate) struct ResolvedToolEvidence<'a> {
    pub(crate) call: &'a ProjectedEvidenceCall,
    pub(crate) text: &'a str,
}

impl ResolvedToolEvidence<'_> {
    pub(crate) fn searchable_text(&self) -> String {
        format!(
            "{}\n{}",
            serde_json::to_string(&self.call.arguments).unwrap_or_default(),
            self.text
        )
    }
}

impl ToolEvidenceProjection {
    pub(crate) fn new(
        calls: &[ToolCall],
        agent_message_ids: &[String],
        assertion_message_ids: &[String],
        lookback_messages: usize,
        result_token_cap: usize,
        is_memory_path: impl Fn(&str) -> bool + Copy,
    ) -> Self {
        let positions = agent_message_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let assertions = assertion_message_ids.iter().collect::<HashSet<_>>();
        let lookback_messages = lookback_messages.max(1);
        let mut horizon_messages = HashSet::new();
        for assertion in &assertions {
            let Some(&end) = positions.get(assertion.as_str()) else {
                continue;
            };
            let start = (end + 1).saturating_sub(lookback_messages);
            horizon_messages.extend(agent_message_ids[start..=end].iter().map(String::as_str));
        }
        let mut eligible = calls
            .iter()
            .filter(|call| call.success && horizon_messages.contains(call.message_id.as_str()))
            .collect::<Vec<_>>();
        eligible.sort_by(|a, b| {
            (
                positions[a.message_id.as_str()],
                a.turn,
                a.created_at,
                &a.id,
            )
                .cmp(&(
                    positions[b.message_id.as_str()],
                    b.turn,
                    b.created_at,
                    &b.id,
                ))
        });

        let mut projection = Self {
            result_token_cap: result_token_cap.max(1),
            ..Self::default()
        };
        let mut browser_urls = HashMap::new();
        for (index, call) in eligible.into_iter().enumerate() {
            let class = classify_execution(call, is_memory_path);
            let local_id = format!("e{}", index + 1);
            let arguments = sanitize_arguments(&call.arguments);
            let (query, url) = source_metadata(call, &arguments, &mut browser_urls);
            projection.by_local_id.insert(
                local_id.clone(),
                ProjectedEvidenceCall {
                    local_id,
                    tool_call_id: call.id.clone(),
                    chat_id: call.chat_id.clone(),
                    message_id: call.message_id.clone(),
                    name: call.name.clone(),
                    arguments,
                    result: sanitize_text(&call.result),
                    query,
                    url,
                    class,
                    chunks: evidence_chunks(&sanitize_text(&call.result)),
                },
            );
        }
        for assertion in assertions {
            let Some(&end) = positions.get(assertion.as_str()) else {
                continue;
            };
            let start = (end + 1).saturating_sub(lookback_messages);
            let horizon = &agent_message_ids[start..=end];
            let mut ids = projection
                .by_local_id
                .values()
                .filter(|call| horizon.iter().any(|id| id == &call.message_id))
                .map(|call| call.local_id.clone())
                .collect::<Vec<_>>();
            ids.sort_by_key(|id| {
                id.trim_start_matches('e')
                    .parse::<usize>()
                    .unwrap_or(usize::MAX)
            });
            projection
                .by_assertion_message
                .insert(assertion.clone(), ids);
        }
        projection
    }

    pub(crate) fn new_with_task_evidence(
        calls: &[ToolCall],
        agent_message_ids: &[String],
        assertion_message_ids: &[String],
        task_calls_by_assertion: &HashMap<String, Vec<ToolCall>>,
        lookback_messages: usize,
        result_token_cap: usize,
        is_memory_path: impl Fn(&str) -> bool + Copy,
    ) -> Self {
        let mut projection = Self::new(
            calls,
            agent_message_ids,
            assertion_message_ids,
            lookback_messages,
            result_token_cap,
            is_memory_path,
        );
        let mut local_by_tool_call = projection
            .by_local_id
            .values()
            .map(|call| (call.tool_call_id.clone(), call.local_id.clone()))
            .collect::<HashMap<_, _>>();
        let mut next_local = projection.by_local_id.len() + 1;
        let mut assertions = task_calls_by_assertion.keys().collect::<Vec<_>>();
        assertions.sort();
        for assertion in assertions {
            let mut calls = task_calls_by_assertion[assertion]
                .iter()
                .filter(|call| call.success)
                .collect::<Vec<_>>();
            calls.sort_by(|left, right| {
                (left.created_at, &left.chat_id, left.turn, &left.id).cmp(&(
                    right.created_at,
                    &right.chat_id,
                    right.turn,
                    &right.id,
                ))
            });
            let linked = projection
                .by_assertion_message
                .entry(assertion.clone())
                .or_default();
            let mut browser_urls = HashMap::new();
            for call in calls {
                let arguments = sanitize_arguments(&call.arguments);
                let (query, url) = source_metadata(call, &arguments, &mut browser_urls);
                let local_id = if let Some(existing) = local_by_tool_call.get(&call.id) {
                    existing.clone()
                } else {
                    let local_id = format!("e{next_local}");
                    next_local += 1;
                    let result = sanitize_text(&call.result);
                    projection.by_local_id.insert(
                        local_id.clone(),
                        ProjectedEvidenceCall {
                            local_id: local_id.clone(),
                            tool_call_id: call.id.clone(),
                            chat_id: call.chat_id.clone(),
                            message_id: call.message_id.clone(),
                            name: call.name.clone(),
                            arguments,
                            result: result.clone(),
                            query,
                            url,
                            class: classify_execution(call, is_memory_path),
                            chunks: evidence_chunks(&result),
                        },
                    );
                    local_by_tool_call.insert(call.id.clone(), local_id.clone());
                    local_id
                };
                if !linked.contains(&local_id) {
                    linked.push(local_id);
                }
            }
        }
        projection
    }

    pub(crate) fn qualified_for_message(&self, message_id: &str) -> Vec<&ProjectedEvidenceCall> {
        self.by_assertion_message
            .get(message_id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.by_local_id.get(id))
            .filter(|call| call.class == ExecutionEvidenceClass::Evidence)
            .collect()
    }

    pub(crate) fn has_direct_evidence(&self, message_id: &str) -> bool {
        self.by_assertion_message
            .get(message_id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.by_local_id.get(id))
            .any(|call| call.class == ExecutionEvidenceClass::Evidence)
    }

    pub(crate) fn strong_match_for_message(
        &self,
        message_id: &str,
        claim: &str,
    ) -> Option<&ProjectedEvidenceCall> {
        self.qualified_for_message(message_id)
            .into_iter()
            .find(|call| {
                call.chunks
                    .iter()
                    .any(|chunk| normalized_containment(claim, chunk))
            })
    }

    pub(crate) fn ranked_for_message(
        &self,
        message_id: &str,
        claim: &str,
        quote: &str,
    ) -> Vec<&ProjectedEvidenceCall> {
        let calls = self.qualified_for_message(message_id);
        let mut scores: HashMap<&str, f64> = HashMap::new();
        let rankings = [
            ranked_by(&calls, |call| {
                bm25_like_score(claim, &call.searchable_text())
            }),
            ranked_by(&calls, |call| {
                bm25_like_score(quote, &call.searchable_text())
            }),
            ranked_by(&calls, |call| {
                critical_value_score(claim, &call.searchable_text())
            }),
            ranked_by(&calls, |call| {
                coverage_score(claim, &call.searchable_text())
            }),
        ];
        for ranked in rankings {
            for (rank, (call, score)) in ranked.into_iter().enumerate() {
                if score > 0.0 {
                    *scores.entry(&call.local_id).or_default() += 1.0 / (60.0 + rank as f64 + 1.0);
                }
            }
        }
        let mut ranked = calls;
        ranked.retain(|call| {
            scores
                .get(call.local_id.as_str())
                .is_some_and(|score| *score > 0.0)
        });
        ranked.sort_by(|a, b| {
            scores
                .get(b.local_id.as_str())
                .unwrap_or(&0.0)
                .total_cmp(scores.get(a.local_id.as_str()).unwrap_or(&0.0))
                .then(a.local_id.cmp(&b.local_id))
        });
        ranked
    }

    fn evidence_id(&self, prompt_message: &str, call_local_id: &str, chunk_index: usize) -> String {
        let mut handles = self.handles.lock().expect("tool evidence handles poisoned");
        let target = (call_local_id.to_string(), chunk_index);
        if let Some(existing) = handles.by_target.get(&target) {
            return existing.clone();
        }
        let id = format!("{call_local_id}:chunk{}", chunk_index + 1);
        let resolved = EvidenceHandleTarget {
            call_local_id: call_local_id.to_string(),
            chunk_index,
        };
        handles.by_target.insert(target, id.clone());
        handles.by_id.insert(id.clone(), resolved.clone());

        // Keep old extraction conversations resumable. New search results only expose
        // the stable ID above, but an in-flight model can still submit the former
        // prompt-scoped handle that it already received.
        let next = handles
            .next_by_prompt_message
            .entry(prompt_message.to_string())
            .or_default();
        *next += 1;
        let legacy_id = format!("{prompt_message}:tool{next}");
        handles.by_id.insert(legacy_id, resolved);
        id
    }

    pub(crate) fn resolve_evidence_id(
        &self,
        _prompt_message: &str,
        assertion_message_id: &str,
        evidence_id: &str,
    ) -> Option<ResolvedToolEvidence<'_>> {
        let target = self
            .handles
            .lock()
            .expect("tool evidence handles poisoned")
            .by_id
            .get(evidence_id)
            .cloned()?;
        let call = self.by_local_id.get(&target.call_local_id)?;
        if !self
            .by_assertion_message
            .get(assertion_message_id)?
            .iter()
            .any(|id| id == &target.call_local_id)
        {
            return None;
        }
        Some(ResolvedToolEvidence {
            call,
            text: call.chunks.get(target.chunk_index)?,
        })
    }

    pub(crate) fn evidence_id_for_quote(
        &self,
        _prompt_message: &str,
        _assertion_message_id: &str,
        call_local_id: &str,
        quote: &str,
    ) -> Option<String> {
        let handles = self.handles.lock().expect("tool evidence handles poisoned");
        handles.by_id.iter().find_map(|(id, target)| {
            if target.call_local_id != call_local_id || id.contains(":tool") {
                return None;
            }
            let call = self.by_local_id.get(&target.call_local_id)?;
            let text = call.chunks.get(target.chunk_index)?;
            GroundingText::new(&format!(
                "{}\n{text}",
                serde_json::to_string(&call.arguments).ok()?
            ))
            .resolve(quote)
            .is_ok()
            .then(|| id.clone())
        })
    }

    #[cfg(test)]
    pub(crate) fn search_for_message(
        &self,
        prompt_message: &str,
        message_id: &str,
        query: &str,
        token_cap: usize,
    ) -> String {
        self.search_for_message_with_handles(
            prompt_message,
            message_id,
            query,
            token_cap,
            &HashMap::new(),
        )
    }

    fn search_for_message_with_handles(
        &self,
        prompt_message: &str,
        message_id: &str,
        query: &str,
        token_cap: usize,
        source_handles: &HashMap<String, String>,
    ) -> String {
        let available = self.qualified_for_message(message_id);
        let corpus = available
            .iter()
            .map(|call| call.critical_value_text())
            .collect::<Vec<_>>()
            .join("\n");
        let critical = critical_values(query);
        if !critical.is_empty() && missing_critical_values(query, &corpus).len() == critical.len() {
            return "{\"results\":[]}".to_string();
        }
        let mut results = Vec::new();
        for call in self.ranked_for_message(message_id, query, query) {
            let mut chunks = call
                .chunks
                .iter()
                .enumerate()
                .map(|(index, text)| (index, text, bm25_like_score(query, text)))
                .filter(|(_, _, score)| *score > 0.0)
                .collect::<Vec<_>>();
            chunks.sort_by(|(_, _, a), (_, _, b)| b.total_cmp(a));
            if chunks.is_empty() {
                chunks.push((
                    0,
                    &call.result,
                    bm25_like_score(query, &call.searchable_text()),
                ));
            }
            for (chunk_index, text, score) in chunks {
                let evidence_id = self.evidence_id(prompt_message, &call.local_id, chunk_index);
                let candidate = serde_json::json!({
                    "message": prompt_message,
                    "source_message": source_handles.get(&call.message_id)
                        .map(String::as_str).unwrap_or(prompt_message),
                    "evidence_id": evidence_id,
                    "tool": call.name,
                    "score": score,
                    "request": call.arguments,
                    "text": bounded(text, 2_000),
                });
                let mut with_candidate = results.clone();
                with_candidate.push(candidate.clone());
                let rendered =
                    serde_json::to_string_pretty(&serde_json::json!({ "results": with_candidate }))
                        .unwrap_or_default();
                if crate::inference::context::estimate_tokens(&rendered) > token_cap {
                    break;
                }
                results.push(candidate);
            }
        }
        serde_json::to_string_pretty(&serde_json::json!({ "results": results }))
            .unwrap_or_else(|_| "{\"results\":[]}".to_string())
    }
}

pub(crate) fn classify_execution(
    call: &ToolCall,
    is_memory_path: impl Fn(&str) -> bool,
) -> ExecutionEvidenceClass {
    if call
        .hitl
        .as_ref()
        .is_some_and(|hitl| hitl.status == crate::inference::tool_call::ToolStatus::Resolved)
    {
        return ExecutionEvidenceClass::UserResponse;
    }
    if matches!(
        call.name.as_str(),
        "request_credentials" | "get_secret" | "read_secret"
    ) {
        return ExecutionEvidenceClass::Sensitive;
    }
    if matches!(
        call.name.as_str(),
        "create_task"
            | "create_recurring_task"
            | "complete_task"
            | "fail_task"
            | "defer_task"
            | "cancel_task"
    ) {
        return ExecutionEvidenceClass::TaskControl;
    }
    if let Some(class) = classify_memory_operation(&call.name) {
        return class;
    }
    let argument_strings = strings_in_value(&call.arguments);
    if matches!(call.name.as_str(), "shell" | "mcpctl" | "exec" | "execute")
        && let Some(class) = argument_strings
            .iter()
            .find_map(|value| classify_memory_operation(value))
    {
        return class;
    }
    if matches!(call.name.as_str(), "read" | "grep" | "glob" | "shell")
        && argument_strings.iter().any(|value| is_memory_path(value))
    {
        return ExecutionEvidenceClass::Recall;
    }
    ExecutionEvidenceClass::Evidence
}

fn classify_memory_operation(value: &str) -> Option<ExecutionEvidenceClass> {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    let words = normalized.split_whitespace().collect::<Vec<_>>();
    let compact = words.join("");
    let has_nearby = |actions: &[&str]| {
        words.iter().enumerate().any(|(memory_index, word)| {
            *word == "memory"
                && words.iter().enumerate().any(|(action_index, word)| {
                    actions.contains(word) && memory_index.abs_diff(action_index) <= 2
                })
        })
    };
    let mutation = ["remember", "write", "store", "delete"];
    let recall = ["search", "read", "get", "cite", "citation", "query"];
    if value.eq_ignore_ascii_case("remember")
        || has_nearby(&mutation)
        || mutation.iter().any(|action| {
            compact.contains(&format!("memory{action}"))
                || compact.contains(&format!("{action}memory"))
        })
    {
        Some(ExecutionEvidenceClass::MemoryMutation)
    } else if has_nearby(&recall)
        || recall.iter().any(|action| {
            compact.contains(&format!("memory{action}"))
                || compact.contains(&format!("{action}memory"))
        })
    {
        Some(ExecutionEvidenceClass::Recall)
    } else {
        None
    }
}

pub(crate) fn sanitize_arguments(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter_map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    (!matches!(
                        lower.as_str(),
                        "authorization"
                            | "proxy-authorization"
                            | "cookie"
                            | "set-cookie"
                            | "token"
                            | "access_token"
                            | "refresh_token"
                            | "api_key"
                            | "apikey"
                            | "password"
                            | "secret"
                            | "binary"
                            | "bytes"
                            | "base64"
                            | "image_data"
                            | "audio_data"
                    ))
                    .then(|| (key.clone(), sanitize_arguments(value)))
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(sanitize_arguments).collect()),
        Value::String(value) => Value::String(sanitize_text(value)),
        value => value.clone(),
    }
}

pub(crate) fn normalized_containment(claim: &str, evidence: &str) -> bool {
    let claim = normalize(claim);
    let evidence = normalize(evidence);
    if claim.split_whitespace().count() < 3 {
        return false;
    }
    evidence.contains(&claim)
}

pub(crate) fn missing_critical_values<'a>(claim: &'a str, evidence: &str) -> Vec<&'a str> {
    let grounding = GroundingText::new(evidence);
    critical_values(claim)
        .into_iter()
        .filter(|token| grounding.resolve_value(token).is_err())
        .collect()
}

fn critical_values(value: &str) -> Vec<&str> {
    let mut seen = HashSet::new();
    value
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|token| {
            token.chars().any(|ch| ch.is_ascii_digit())
                || token.starts_with("http://")
                || token.starts_with("https://")
                || token.contains('_')
        })
        .filter(|token| seen.insert(*token))
        .collect()
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings_in_value(value: &Value) -> Vec<&str> {
    match value {
        Value::String(value) => vec![value],
        Value::Array(values) => values.iter().flat_map(strings_in_value).collect(),
        Value::Object(values) => values.values().flat_map(strings_in_value).collect(),
        _ => Vec::new(),
    }
}

fn sanitize_text(value: &str) -> String {
    rtb_redact::string(value).into_owned()
}

fn source_metadata(
    call: &ToolCall,
    arguments: &Value,
    browser_urls: &mut HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .map(str::to_string);
    let requested_url = direct_requested_url(arguments).or_else(|| curl_requested_url(arguments));
    let url = browser_source_url(call, requested_url, browser_urls);
    (query, url)
}

fn direct_requested_url(arguments: &Value) -> Option<String> {
    arguments
        .get("url")
        .or_else(|| arguments.get("href"))
        .and_then(Value::as_str)
        .filter(|value| is_full_web_url(value))
        .map(str::to_string)
}

fn browser_source_url(
    call: &ToolCall,
    requested_url: Option<String>,
    browser_urls: &mut HashMap<String, String>,
) -> Option<String> {
    let name = call.name.to_ascii_lowercase();
    if !name.starts_with("browser_") {
        return requested_url;
    }
    match name.as_str() {
        "browser_navigate" | "browser_new_tab" => {
            if let Some(url) = requested_url.as_ref() {
                browser_urls.insert(call.chat_id.clone(), url.clone());
            } else {
                browser_urls.remove(&call.chat_id);
            }
            requested_url
        }
        "browser_extract" | "browser_get_markdown" | "browser_read_links" | "browser_snapshot" => {
            requested_url.or_else(|| browser_urls.get(&call.chat_id).cloned())
        }
        "browser_click" | "browser_close" | "browser_close_tab" | "browser_evaluate"
        | "browser_go_back" | "browser_go_forward" | "browser_input_fill" | "browser_press_key"
        | "browser_select" | "browser_switch_tab" => {
            browser_urls.remove(&call.chat_id);
            requested_url
        }
        _ => requested_url,
    }
}

fn curl_requested_url(arguments: &Value) -> Option<String> {
    let command = arguments
        .get("command")
        .or_else(|| arguments.get("cmd"))
        .and_then(Value::as_str)?;
    let tokens = shell_tokens(command);
    let curl_index = tokens
        .iter()
        .position(|token| token.rsplit('/').next().is_some_and(|name| name == "curl"))?;
    let mut urls = tokens[curl_index + 1..]
        .iter()
        .take_while(|token| !matches!(token.as_str(), ";" | "|" | "&" | "&&" | "||"))
        .filter_map(|token| token.strip_prefix("--url=").or(Some(token.as_str())))
        .filter(|token| is_full_web_url(token))
        .map(str::to_string)
        .collect::<Vec<_>>();
    urls.sort();
    urls.dedup();
    (urls.len() == 1).then(|| urls.remove(0))
}

fn is_full_web_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
            continue;
        }
        match quote {
            Some(active) if ch == active => quote = None,
            Some('\'') => token.push(ch),
            Some(_) if ch == '\\' => escaped = true,
            Some(_) => token.push(ch),
            None if ch == '\\' => escaped = true,
            None if matches!(ch, '\'' | '"') => quote = Some(ch),
            None if ch.is_whitespace() => push_shell_token(&mut tokens, &mut token),
            None if matches!(ch, ';' | '|' | '&') => {
                push_shell_token(&mut tokens, &mut token);
                tokens.push(ch.to_string());
            }
            None => token.push(ch),
        }
    }
    if escaped {
        token.push('\\');
    }
    push_shell_token(&mut tokens, &mut token);
    tokens
}

fn push_shell_token(tokens: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        tokens.push(std::mem::take(token));
    }
}

fn coverage_score(query: &str, text: &str) -> f64 {
    let query = normalize(query)
        .split_whitespace()
        .map(str::to_string)
        .collect::<HashSet<_>>();
    if query.is_empty() {
        return 0.0;
    }
    let text = normalize(text)
        .split_whitespace()
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let coverage =
        query.iter().filter(|token| text.contains(*token)).count() as f64 / query.len() as f64;
    coverage + if coverage >= 0.8 { 0.25 } else { 0.0 }
}

fn bm25_like_score(query: &str, text: &str) -> f64 {
    let query_tokens = normalize(query)
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if query_tokens.is_empty() {
        return 0.0;
    }
    let text_tokens = normalize(text)
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut score = 0.0;
    for token in &query_tokens {
        let frequency = text_tokens
            .iter()
            .filter(|candidate| *candidate == token)
            .count() as f64;
        if frequency > 0.0 {
            score += 1.0 + frequency.ln();
        }
    }
    score / (query_tokens.len() as f64 * (1.0 + text_tokens.len() as f64 / 400.0))
}

fn critical_value_score(query: &str, text: &str) -> f64 {
    let critical = query
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|token| {
            token.chars().any(|ch| ch.is_ascii_digit())
                || token.starts_with("http://")
                || token.starts_with("https://")
                || token.contains('_')
        })
        .collect::<Vec<_>>();
    if critical.is_empty() {
        return 0.0;
    }
    critical
        .iter()
        .filter(|token| text.contains(**token))
        .count() as f64
        / critical.len() as f64
}

fn ranked_by<'a>(
    calls: &[&'a ProjectedEvidenceCall],
    score: impl Fn(&ProjectedEvidenceCall) -> f64,
) -> Vec<(&'a ProjectedEvidenceCall, f64)> {
    let mut ranked = calls
        .iter()
        .map(|call| (*call, score(call)))
        .collect::<Vec<_>>();
    ranked.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then(a.local_id.cmp(&b.local_id)));
    ranked
}

fn evidence_chunks(result: &str) -> Vec<String> {
    if let Ok(value) = serde_json::from_str::<Value>(result) {
        match value {
            Value::Array(values) => {
                return values
                    .into_iter()
                    .filter_map(|value| serde_json::to_string(&value).ok())
                    .collect();
            }
            Value::Object(map) => {
                let records = map
                    .values()
                    .filter_map(|value| match value {
                        Value::Array(values) => Some(
                            values
                                .iter()
                                .filter_map(|value| serde_json::to_string(value).ok())
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>();
                if !records.is_empty() {
                    return records;
                }
                return vec![serde_json::to_string(&Value::Object(map)).unwrap_or_default()];
            }
            value => return vec![value.to_string()],
        }
    }
    let paragraphs = result
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let logical = if paragraphs.len() > 1 {
        paragraphs
    } else {
        result.lines().collect()
    };
    let mut chunks = Vec::new();
    let mut current = String::new();
    for part in logical {
        let candidate = if current.is_empty() {
            part.to_string()
        } else {
            format!("{current}\n{part}")
        };
        if !current.is_empty() && crate::inference::context::estimate_tokens(&candidate) > 500 {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(part);
        if crate::inference::context::estimate_tokens(&current) >= 200 {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn bounded(value: &str, char_cap: usize) -> String {
    if value.chars().count() <= char_cap {
        value.to_string()
    } else {
        value
            .chars()
            .take(char_cap.saturating_sub(1))
            .chain(std::iter::once('…'))
            .collect()
    }
}

pub(crate) struct SearchToolEvidenceTool {
    pub prompts: PromptLoader,
    pub projection: ToolEvidenceProjection,
    pub messages: HashMap<String, String>,
    pub token_cap: usize,
    pub lookups: Arc<AtomicUsize>,
    pub max_lookups: usize,
    pub searched_messages: Arc<Mutex<HashSet<String>>>,
}

#[agent_tool(name = "search_tool_evidence", dir = "pkm")]
impl SearchToolEvidenceTool {
    async fn execute(
        &self,
        _tool_name: &str,
        arguments: Value,
        _ctx: &InferenceContext,
    ) -> Result<ToolOutput, AppError> {
        if self.lookups.fetch_add(1, Ordering::Relaxed) >= self.max_lookups {
            return Ok(ToolOutput::text(
                "The extraction evidence lookup budget is exhausted.",
            ));
        }
        let Some(handle) = str_arg(&arguments, "message_id") else {
            return Ok(ToolOutput::text("Provide an Agent message_id."));
        };
        let Some(message_id) = self.messages.get(handle) else {
            return Ok(ToolOutput::text("Unknown Agent message_id."));
        };
        let Some(query) = str_arg(&arguments, "query") else {
            return Ok(ToolOutput::text("Provide a search query."));
        };
        self.searched_messages
            .lock()
            .expect("searched evidence messages poisoned")
            .insert(handle.to_string());
        let source_handles = self
            .messages
            .iter()
            .map(|(handle, message_id)| (message_id.clone(), handle.clone()))
            .collect::<HashMap<_, _>>();
        Ok(ToolOutput::text(
            self.projection.search_for_message_with_handles(
                handle,
                message_id,
                query,
                self.token_cap,
                &source_handles,
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    fn prompts() -> PromptLoader {
        PromptLoader::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("resources")
                .join("prompts"),
        )
    }

    #[test]
    fn evidence_search_uses_message_ids_and_returns_citable_chunks() {
        let projection = ToolEvidenceProjection::default();
        let lookups = Arc::new(AtomicUsize::new(0));
        let search = SearchToolEvidenceTool {
            prompts: prompts(),
            projection: projection.clone(),
            messages: HashMap::new(),
            token_cap: 4_000,
            lookups: lookups.clone(),
            max_lookups: 10,
            searched_messages: Arc::new(Mutex::new(HashSet::new())),
        };

        let search_definitions = crate::tool::AgentTool::definitions(&search);

        assert_eq!(search_definitions.len(), 1);
        assert_eq!(search_definitions[0].id, "search_tool_evidence");
        assert!(
            search_definitions[0].parameters["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "message_id"))
        );
        assert!(
            !search_definitions[0].parameters["properties"]
                .as_object()
                .is_some_and(|properties| properties.contains_key("assertion"))
        );
    }

    fn call(name: &str, arguments: Value, result: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            chat_id: "chat-1".into(),
            message_id: "agent-1".into(),
            turn: 1,
            provider_call_id: "provider-1".into(),
            name: name.into(),
            arguments,
            result: result.into(),
            success: true,
            duration_ms: 1,
            hitl: None,
            task_event: None,
            system_prompt: None,
            description: None,
            turn_text: None,
            signature: None,
            turn_reasoning: None,
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
    fn classifies_relocated_user_pkm_reads_as_recall() {
        let mut config = crate::core::config::Config::default();
        config.storage.data_dir = "/example/data".into();
        let storage = crate::storage::StorageService::new(&config);
        let handle = crate::handle!("testuser");
        let user_pkm_path = |path: &str| storage.is_user_pkm_path(&handle, path);

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
}
