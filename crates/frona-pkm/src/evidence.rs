use crate::redaction::sanitize_arguments;
use frona_text::redaction::redact_text;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use frona_text::GroundingText;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

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
    pub fn new(
        calls: &[ToolExecution],
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
                    result: redact_text(&call.result),
                    query,
                    url,
                    class,
                    chunks: evidence_chunks(&redact_text(&call.result)),
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

    pub fn new_with_task_evidence(
        calls: &[ToolExecution],
        agent_message_ids: &[String],
        assertion_message_ids: &[String],
        task_calls_by_assertion: &HashMap<String, Vec<ToolExecution>>,
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
                    let result = redact_text(&call.result);
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

    pub(crate) fn search_for_message_with_handles(
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
                if frona_harness::inference::context::estimate_tokens(&rendered) > token_cap {
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
    call: &ToolExecution,
    is_memory_path: impl Fn(&str) -> bool,
) -> ExecutionEvidenceClass {
    if call.user_response {
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

fn source_metadata(
    call: &ToolExecution,
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
    call: &ToolExecution,
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
        if !current.is_empty()
            && frona_harness::inference::context::estimate_tokens(&candidate) > 500
        {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(part);
        if frona_harness::inference::context::estimate_tokens(&current) >= 200 {
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

/// A completed execution supplied by the host. IDs are opaque provenance strings.
/// Supply only calls in the authorized conversation/task evidence window.
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub id: String,
    pub chat_id: String,
    pub message_id: String,
    pub turn: u32,
    pub name: String,
    pub arguments: Value,
    pub result: String,
    pub success: bool,
    /// Whether this execution carries a resolved human response.
    pub user_response: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests;
