use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::agent::prompt::PromptLoader;
use crate::agent::service::AgentService;
use crate::agent::task::executor::TaskExecutor;
use crate::agent::task::models::{Task, TaskKind, TaskStatus};
use crate::agent::task::service::TaskService;
use crate::contact::service::ContactService;
use crate::core::error::AppError;
use crate::policy::models::{PolicyAction, PolicyContact};
use crate::policy::service::PolicyService;

use super::matcher::{Matcher, MatcherKind};
use super::matchers::{CategoryMatcher, ChannelMatcher, ContactMatcher};
use super::models::{Annotation, CandidateEvent, SignalOutput, Watch};

type WatchIndex = HashMap<String, HashMap<String, Watch>>;

pub struct SignalService {
    watches: RwLock<WatchIndex>,
    matchers: Vec<Arc<dyn Matcher>>,
    task_service: TaskService,
    task_executor: Arc<TaskExecutor>,
    agent_service: AgentService,
    contact_service: ContactService,
    policy_service: PolicyService,
    prompts: PromptLoader,
    usage_service: crate::inference::usage::UsageService,
}

impl SignalService {
    pub fn new(
        task_service: TaskService,
        task_executor: Arc<TaskExecutor>,
        agent_service: AgentService,
        contact_service: ContactService,
        policy_service: PolicyService,
        prompts: PromptLoader,
        usage_service: crate::inference::usage::UsageService,
    ) -> Self {
        let matchers: Vec<Arc<dyn Matcher>> = vec![
            Arc::new(CategoryMatcher),
            Arc::new(ChannelMatcher),
            Arc::new(ContactMatcher),
        ];
        Self::with_matchers(
            task_service,
            task_executor,
            agent_service,
            contact_service,
            policy_service,
            prompts,
            usage_service,
            matchers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_matchers(
        task_service: TaskService,
        task_executor: Arc<TaskExecutor>,
        agent_service: AgentService,
        contact_service: ContactService,
        policy_service: PolicyService,
        prompts: PromptLoader,
        usage_service: crate::inference::usage::UsageService,
        matchers: Vec<Arc<dyn Matcher>>,
    ) -> Self {
        Self {
            watches: RwLock::new(HashMap::new()),
            matchers,
            task_service,
            task_executor,
            agent_service,
            contact_service,
            policy_service,
            prompts,
            usage_service,
        }
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), AppError> {
        self.rebuild_from_db().await?;
        Ok(())
    }

    async fn rebuild_from_db(&self) -> Result<(), AppError> {
        let tasks = self.task_service.list_pending_signal_tasks().await?;
        let mut index = self.watches.write().await;
        index.clear();
        for task in tasks {
            if let Some(watch) = Watch::from_task(&task) {
                index
                    .entry(watch.user_id.clone())
                    .or_default()
                    .insert(watch.task_id.clone(), watch);
            }
        }
        Ok(())
    }

    pub async fn register(&self, watch: Watch) {
        let mut index = self.watches.write().await;
        index
            .entry(watch.user_id.clone())
            .or_default()
            .insert(watch.task_id.clone(), watch);
    }

    pub async fn unregister(&self, user_id: &str, task_id: &str) {
        let mut index = self.watches.write().await;
        if let Some(user_watches) = index.get_mut(user_id) {
            user_watches.remove(task_id);
            if user_watches.is_empty() {
                index.remove(user_id);
            }
        }
    }

    pub async fn evaluate(
        &self,
        user_id: &str,
        candidate: CandidateEvent,
    ) -> Result<Vec<String>, AppError> {
        let watches: Vec<Watch> = {
            let index = self.watches.read().await;
            index
                .get(user_id)
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default()
        };

        let mut fired = Vec::new();
        for watch in watches {
            if !self.matches_watch(&candidate, &watch) {
                continue;
            }
            if !self.policy_allows(&candidate, &watch).await? {
                tracing::info!(
                    task_id = %watch.task_id,
                    agent_id = %watch.agent_id,
                    channel = ?candidate.channel.as_ref().map(|c| c.provider.as_str()),
                    sender = ?candidate.sender,
                    "Signal match denied by policy"
                );
                continue;
            }
            match self.fire_signal(&watch, &candidate).await {
                Ok(true) => fired.push(watch.task_id.clone()),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    task_id = %watch.task_id,
                    error = %e,
                    "Failed to fire signal",
                ),
            }
        }
        Ok(fired)
    }

    fn matches_watch(&self, candidate: &CandidateEvent, watch: &Watch) -> bool {
        evaluate_match(&self.matchers, candidate, watch)
    }

    async fn policy_allows(
        &self,
        candidate: &CandidateEvent,
        watch: &Watch,
    ) -> Result<bool, AppError> {
        let agent = self.agent_service.find_by_id(&watch.agent_id).await?;
        let Some(agent) = agent else {
            return Ok(false);
        };

        let address = candidate.sender.clone().unwrap_or_default();
        let paired_addresses: Vec<String> = Vec::new();

        let sender_contact = match candidate.contact.as_ref() {
            Some(c) => PolicyContact {
                id: c.id.clone(),
                user_id: watch.user_id.clone(),
                name: c.name.clone(),
                address: address.clone(),
                addresses: [c.phone.clone(), c.email.clone()]
                    .into_iter()
                    .flatten()
                    .collect(),
            },
            None => PolicyContact::unresolved(&watch.user_id, &address),
        };

        let channel_handle = candidate
            .channel
            .as_ref()
            .map(|c| c.handle.clone())
            .ok_or_else(|| AppError::Internal("Signal candidate missing channel".into()))?;
        let connector_id = candidate
            .chat
            .as_ref()
            .and_then(|c| c.space_id.clone())
            .unwrap_or_default();
        let action = PolicyAction::ReceiveSignal {
            connector_id,
            channel_handle,
            sender: sender_contact,
            paired_addresses,
        };
        let decision = self
            .policy_service
            .authorize(&watch.user_id, &agent, action)
            .await?;
        Ok(decision.allowed)
    }

    async fn fire_signal(
        &self,
        watch: &Watch,
        candidate: &CandidateEvent,
    ) -> Result<bool, AppError> {
        let Some(mut task) = self.task_service.find_by_id(&watch.task_id).await? else {
            self.unregister(&watch.user_id, &watch.task_id).await;
            return Ok(false);
        };
        if !matches!(task.status, TaskStatus::Pending | TaskStatus::InProgress) {
            self.unregister(&watch.user_id, &watch.task_id).await;
            return Ok(false);
        }

        let next_count = bump_evaluation_count(&mut task);
        if next_count > watch.max_evaluations.max(1) {
            match watch.mode {
                super::super::task::models::SignalMode::Continuous => {
                    self.task_service
                        .mark_completed(&task.id, Some("max matches reached".into()))
                        .await?;
                }
                super::super::task::models::SignalMode::Once => {
                    self.task_service
                        .mark_failed(&task.id, "exceeded evaluation budget".into())
                        .await?;
                }
            }
            self.unregister(&watch.user_id, &watch.task_id).await;
            return Ok(false);
        }
        self.task_service.save(&task).await?;

        let injected_message = self.build_candidate_block(candidate, watch.mode);
        let exec = self.task_executor.clone();
        tokio::spawn(async move {
            if let Err(e) = exec
                .run_with_injected_message(&task, injected_message)
                .await
            {
                tracing::error!(error = %e, "Signal task execution failed");
            }
        });
        Ok(true)
    }

    pub async fn watch_count(&self, user_id: &str) -> usize {
        let index = self.watches.read().await;
        index.get(user_id).map(|m| m.len()).unwrap_or(0)
    }

    pub async fn pending_category_hints(&self, user_id: &str) -> Vec<(String, String)> {
        let index = self.watches.read().await;
        let Some(user_watches) = index.get(user_id) else {
            return Vec::new();
        };
        aggregate_category_hints(user_watches.values())
    }

    /// Emits `dispatch_mode = Signal`; the outbound dispatcher drops it.
    pub async fn process_inbound_extract(
        &self,
        chat_service: &crate::chat::service::ChatService,
        model_providers: &crate::inference::provider::service::ModelProviderService,
        channel: &crate::chat::channel::Channel,
        chat: &crate::chat::models::Chat,
        msg: &crate::chat::message::models::Message,
        awaiting: &[(String, String)],
    ) -> Result<(), AppError> {
        use rig_core::completion::Message as RigMessage;

        let agent_msg = chat_service
            .create_executing_signal_message(&chat.id, &chat.agent_id)
            .await?;

        let Some(agent) = self.agent_service.find_by_id(&chat.agent_id).await? else {
            if let Ok(mut msg) = chat_service
                .get_message(&channel.user_id, &agent_msg.id)
                .await
            {
                msg.content = format!(
                    "Signal extraction skipped: agent {} not found",
                    chat.agent_id
                );
                let _ = chat_service.complete_agent_message(msg).await;
            }
            return Ok(());
        };
        let model_group = model_providers.resolve(&crate::inference::ModelRef(
            agent.model_group.clone().into(),
        ))?;

        let system_prompt = self.compose_signal_prompt(&channel.provider, &chat.id, awaiting);
        let history = vec![RigMessage::user(&msg.content)];
        let usage_ctx = crate::inference::usage::UsageContext::new(
            crate::inference::usage::InferenceKind::Signal {
                agent_id: chat.agent_id.clone(),
                chat_id: chat.id.clone(),
                message_id: msg.id.clone(),
            },
            channel.user_id.clone(),
            agent.model_group.clone(),
        );

        let output: SignalOutput = match crate::inference::structured_inference::<SignalOutput>(
            &model_group,
            &system_prompt,
            history,
            &self.usage_service,
            &usage_ctx,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel.id,
                    chat_id = %chat.id,
                    error = %e,
                    "Signal extraction failed",
                );
                if let Ok(mut msg) = chat_service
                    .get_message(&channel.user_id, &agent_msg.id)
                    .await
                {
                    msg.content = format!("Signal extraction failed: {e}");
                    let _ = chat_service.complete_agent_message(msg).await;
                }
                return Ok(());
            }
        };

        let allowed: Vec<String> = awaiting.iter().map(|(c, _)| c.clone()).collect();
        let kept = filter_categories(output.categories, &allowed);

        let annotator_id = format!("agent:{}", chat.agent_id);
        let mut annotations: Vec<Annotation> = kept
            .iter()
            .map(|c| Annotation::category(annotator_id.clone(), c))
            .collect();
        if let Some(s) = output
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            annotations.push(Annotation::summary(annotator_id.clone(), s));
        }

        let fired = if annotations.is_empty() {
            Vec::new()
        } else {
            let contact = if let Some(ref contact_id) = msg.contact_id {
                self.contact_service
                    .get(&channel.user_id, contact_id)
                    .await
                    .ok()
            } else {
                None
            };
            let candidate = CandidateEvent {
                channel: Some(channel.clone()),
                chat: Some(chat.clone()),
                message: Some(msg.clone()),
                contact,
                sender: msg.from_address.clone(),
                annotations,
                content: msg.content.clone(),
            };
            self.evaluate(&channel.user_id, candidate).await?
        };

        let summary_text = if kept.is_empty() {
            "Annotated: no categories matched.".to_string()
        } else if fired.is_empty() {
            format!(
                "Annotated: {}. No pending signals matched.",
                kept.join(", ")
            )
        } else {
            format!(
                "Annotated: {}. Fired {} signal(s).",
                kept.join(", "),
                fired.len()
            )
        };
        if let Ok(mut msg) = chat_service
            .get_message(&channel.user_id, &agent_msg.id)
            .await
        {
            msg.content = summary_text;
            let _ = chat_service.complete_agent_message(msg).await;
        }
        Ok(())
    }

    fn compose_signal_prompt(
        &self,
        channel: &str,
        chat_id: &str,
        awaiting: &[(String, String)],
    ) -> String {
        let categories_block = if awaiting.is_empty() {
            String::new()
        } else {
            let awaiting_list = awaiting
                .iter()
                .map(|(cat, info)| format!("- {cat}: {info}"))
                .collect::<Vec<_>>()
                .join("\n");
            self.prompts
                .read_with_vars(
                    "channel/categories.md",
                    &[("awaiting_categories", &awaiting_list)],
                )
                .unwrap_or_default()
        };
        let vars: &[(&str, &str)] = &[
            ("channel", channel),
            ("chat_id", chat_id),
            ("categories_block", &categories_block),
        ];
        self.prompts
            .read_with_vars("channel/signal.md", vars)
            .unwrap_or_default()
    }
}

/// Empty `allowed` disables filtering (does NOT match nothing).
fn filter_categories(raw: Vec<String>, allowed: &[String]) -> Vec<String> {
    let allow_set: std::collections::HashSet<&str> = allowed.iter().map(|s| s.as_str()).collect();
    raw.into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .filter(|c| {
            if allow_set.is_empty() || allow_set.contains(c.as_str()) {
                true
            } else {
                tracing::debug!(category = %c, "dropping category not in watched set");
                false
            }
        })
        .collect()
}

fn aggregate_category_hints<'a, I>(watches: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = &'a Watch>,
{
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for watch in watches {
        for cat in &watch.expected_categories {
            *counts.entry(cat.clone()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .map(|(cat, n)| {
            let suffix = if n == 1 {
                "task waiting"
            } else {
                "tasks waiting"
            };
            (cat, format!("{n} {suffix}"))
        })
        .collect()
}

pub fn evaluate_match(
    matchers: &[Arc<dyn Matcher>],
    candidate: &CandidateEvent,
    watch: &Watch,
) -> bool {
    let mut had_scoring_match = false;
    let mut had_active_matcher = false;
    let mut had_active_scoring_matcher = false;

    for matcher in matchers {
        if !matcher.is_active(watch) {
            continue;
        }
        had_active_matcher = true;
        if matcher.kind() == MatcherKind::Scoring {
            had_active_scoring_matcher = true;
        }
        match matcher.evaluate(candidate, watch) {
            None => {
                if matcher.kind() == MatcherKind::HardFilter {
                    return false;
                }
            }
            Some(_score) => {
                had_scoring_match = true;
            }
        }
    }

    if !had_active_matcher {
        return false;
    }
    if !had_active_scoring_matcher {
        return true;
    }
    had_scoring_match
}

fn bump_evaluation_count(task: &mut Task) -> u32 {
    if let TaskKind::Signal {
        ref mut evaluation_count,
        ..
    } = task.kind
    {
        *evaluation_count = evaluation_count.saturating_add(1);
        return *evaluation_count;
    }
    0
}

impl SignalService {
    fn build_candidate_block(
        &self,
        c: &CandidateEvent,
        mode: super::super::task::models::SignalMode,
    ) -> String {
        let channel = c
            .channel
            .as_ref()
            .map(|ch| ch.provider.as_str())
            .unwrap_or("(unknown channel)");
        let sender = c.sender.as_deref().unwrap_or("(unknown sender)");
        let summary = c.summary().unwrap_or("(none)");
        let template = match mode {
            super::super::task::models::SignalMode::Once => "signal_candidate_once.md",
            super::super::task::models::SignalMode::Continuous => "signal_candidate_continuous.md",
        };
        self.prompts
            .read_with_vars(
                template,
                &[
                    ("channel", channel),
                    ("sender", sender),
                    ("content", &c.content),
                    ("summary", summary),
                ],
            )
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::signal::matchers::{CategoryMatcher, ChannelMatcher, ContactMatcher};
    use crate::agent::signal::models::Annotation;
    use chrono::Utc;

    #[test]
    fn filter_categories_drops_blanks_and_trims() {
        let raw = vec!["  alert ".into(), "".into(), "   ".into(), "auth".into()];
        let kept = filter_categories(raw, &[]);
        assert_eq!(kept, vec!["alert".to_string(), "auth".to_string()]);
    }

    #[test]
    fn filter_categories_drops_unwatched_when_allowlist_present() {
        let raw = vec!["alert".into(), "intruder".into(), "auth".into()];
        let allow = vec!["alert".to_string(), "auth".to_string()];
        let kept = filter_categories(raw, &allow);
        assert_eq!(kept, vec!["alert".to_string(), "auth".to_string()]);
    }

    #[test]
    fn filter_categories_keeps_all_when_allowlist_empty() {
        let raw = vec!["anything".into(), "goes".into()];
        let kept = filter_categories(raw.clone(), &[]);
        assert_eq!(kept, raw);
    }

    fn default_matchers() -> Vec<Arc<dyn Matcher>> {
        vec![
            Arc::new(CategoryMatcher),
            Arc::new(ChannelMatcher),
            Arc::new(ContactMatcher),
        ]
    }

    fn make_watch(cats: &[&str], channels: &[&str], contacts: &[&str]) -> Watch {
        Watch {
            task_id: "t".into(),
            user_id: "u".into(),
            agent_id: "a".into(),
            source_chat_id: "c".into(),
            resume_parent: false,
            mode: crate::agent::task::models::SignalMode::Once,
            expected_categories: cats.iter().map(|s| s.to_string()).collect(),
            expected_channels: channels.iter().map(|s| s.to_string()).collect(),
            expected_contacts: contacts.iter().map(|s| s.to_string()).collect(),
            expires_at: None,
            max_evaluations: 50,
            evaluation_count: 0,
        }
    }

    fn make_candidate(
        cats: &[&str],
        channel: Option<&str>,
        contact: Option<&str>,
    ) -> CandidateEvent {
        use crate::agent::signal::models::test_fixtures;
        CandidateEvent {
            channel: channel.map(test_fixtures::channel),
            contact: contact.map(test_fixtures::contact),
            annotations: cats
                .iter()
                .map(|c| Annotation::category("agent:test", *c))
                .collect(),
            ..test_fixtures::candidate()
        }
    }

    #[test]
    fn no_active_matchers_means_no_match() {
        let m = default_matchers();
        let watch = make_watch(&[], &[], &[]);
        let cand = make_candidate(&["verification_code"], Some("sms"), Some("c-1"));
        assert!(!evaluate_match(&m, &cand, &watch));
    }

    #[test]
    fn tag_overlap_alone_fires() {
        let m = default_matchers();
        let watch = make_watch(&["verification_code"], &[], &[]);
        let cand = make_candidate(&["verification_code"], None, None);
        assert!(evaluate_match(&m, &cand, &watch));
    }

    #[test]
    fn hard_filter_rejects_even_when_tag_overlap() {
        let m = default_matchers();
        let watch = make_watch(&["verification_code"], &["sms"], &[]);
        let cand = make_candidate(&["verification_code"], Some("email"), None);
        assert!(!evaluate_match(&m, &cand, &watch));
    }

    #[test]
    fn hard_filter_only_watch_fires_on_filter_pass() {
        let m = default_matchers();
        let watch = make_watch(&[], &["sms"], &[]);
        let cand = make_candidate(&[], Some("sms"), None);
        assert!(evaluate_match(&m, &cand, &watch));
    }

    #[test]
    fn hard_filter_only_watch_rejects_on_filter_fail() {
        let m = default_matchers();
        let watch = make_watch(&[], &["sms"], &[]);
        let cand = make_candidate(&[], Some("email"), None);
        assert!(!evaluate_match(&m, &cand, &watch));
    }

    #[test]
    fn tag_watch_with_no_overlap_does_not_fire() {
        let m = default_matchers();
        let watch = make_watch(&["verification_code"], &[], &[]);
        let cand = make_candidate(&["chitchat"], None, None);
        assert!(!evaluate_match(&m, &cand, &watch));
    }

    #[test]
    fn combined_tag_and_filters_all_must_pass() {
        let m = default_matchers();
        let watch = make_watch(&["verification_code"], &["sms"], &["c-bank"]);
        let cand_ok = make_candidate(&["verification_code"], Some("sms"), Some("c-bank"));
        assert!(evaluate_match(&m, &cand_ok, &watch));

        let cand_wrong_contact =
            make_candidate(&["verification_code"], Some("sms"), Some("c-other"));
        assert!(!evaluate_match(&m, &cand_wrong_contact, &watch));
    }

    #[test]
    fn aggregate_category_hints_dedupes_sorts_and_counts() {
        let w1 = make_watch(&["verification_code", "auth"], &[], &[]);
        let w2 = make_watch(&["verification_code", "bank"], &[], &[]);
        let w3 = make_watch(&[], &["sms"], &[]);
        let hints = aggregate_category_hints([&w1, &w2, &w3]);
        assert_eq!(
            hints,
            vec![
                ("auth".into(), "1 task waiting".into()),
                ("bank".into(), "1 task waiting".into()),
                ("verification_code".into(), "2 tasks waiting".into()),
            ]
        );
    }

    #[test]
    fn aggregate_category_hints_empty_when_no_watches() {
        let hints: Vec<(String, String)> = aggregate_category_hints(std::iter::empty::<&Watch>());
        assert!(hints.is_empty());
    }

    #[test]
    fn bump_evaluation_count_increments_signal_kind() {
        let mut task = Task {
            id: "t".into(),
            user_id: "u".into(),
            agent_id: "a".into(),
            space_id: None,
            chat_id: None,
            title: "x".into(),
            description: "y".into(),
            status: TaskStatus::Pending,
            kind: TaskKind::Signal {
                source_chat_id: "c".into(),
                resume_parent: false,
                mode: crate::agent::task::models::SignalMode::Once,
                expected_categories: vec!["t".into()],
                expected_channels: vec![],
                expected_contacts: vec![],
                expires_at: None,
                max_evaluations: 5,
                evaluation_count: 0,
            },
            run_at: None,
            result_summary: None,
            error_message: None,
            quarantined: false,
            result_schema: None,
            result_description: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(bump_evaluation_count(&mut task), 1);
        assert_eq!(bump_evaluation_count(&mut task), 2);
    }
}
