use super::*;
use crate::{
    agent::{service::AgentService, skill::service::SkillService},
    policy::{models::PolicyAction, service::PolicyService},
    tool::mcp::{McpServerService, models::McpServerStatus},
};
use frona_text::prompt::PromptLoader;

pub struct CatalogContextSource {
    pub agents: AgentService,
    pub skills: SkillService,
    pub mcp: Arc<McpServerService>,
    pub policy: PolicyService,
    pub prompts: PromptLoader,
    pub bridge_mode: bool,
}

#[async_trait]
impl AgentContextSource for CatalogContextSource {
    async fn collect(
        &self,
        scope: &AgentContextScope<'_>,
        _state: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        let user = scope.user;
        let agent = scope.agent;
        if agent.user_id != user.id {
            return Err(AppError::Forbidden("Agent context scope".into()));
        }
        let skills = self
            .skills
            .try_list(&user.handle, &agent.handle, agent.skills.as_deref())
            .await?;
        let mut sections = vec![AgentContextSection::tagged(
            AgentContextKind::Skills,
            "available_skills",
            skills
                .into_iter()
                .filter(|s| !s.disable_model_invocation)
                .map(|s| {
                    (
                        s.name.clone(),
                        format!(
                            "- {}: {} (file: {}/SKILL.md)\n",
                            s.name, s.description, s.path
                        ),
                    )
                })
                .collect(),
        )];
        let mut entries = BTreeMap::new();
        for target in self.agents.list(&scope.user.id).await? {
            if target.id == agent.id || !target.enabled {
                continue;
            }
            let decision = self
                .policy
                .authorize(
                    &scope.user.id,
                    agent,
                    PolicyAction::DelegateTask {
                        target_agent_id: target.id.clone(),
                        target_handle: target.handle.clone(),
                    },
                )
                .await?;
            if decision.allowed {
                entries.insert(
                    target.id,
                    format!("- {}: {}\n", target.name, target.description),
                );
            }
        }
        let mut agents =
            AgentContextSection::tagged(AgentContextKind::Agents, "available_agents", entries);
        if let Some(header) = self.prompts.try_read("AVAILABLE_AGENTS.md")? {
            agents.prefix.push_str(header.trim());
            agents.prefix.push('\n');
        }
        sections.push(agents);
        if self.bridge_mode {
            let mut entries = BTreeMap::new();
            for server in self.mcp.list_for_user(&scope.user.id).await? {
                if server.status != McpServerStatus::Running {
                    continue;
                }
                if self
                    .policy
                    .is_tool_group_permitted(
                        &scope.user.id,
                        &user.handle,
                        &agent.handle,
                        &format!("mcp:{}", server.handle),
                    )
                    .await?
                {
                    entries.insert(
                        server.id,
                        format!(
                            "- {}: {}\n",
                            server.handle,
                            server.description.unwrap_or(server.display_name)
                        ),
                    );
                }
            }
            sections.push(AgentContextSection::tagged(
                AgentContextKind::McpServers,
                "mcpservers",
                entries,
            ));
        }
        // Empty catalogs add no prompt text and do not announce removals.
        sections.retain(|s| !s.entries.is_empty());
        Ok(sections)
    }
}

pub struct MemoryContextSource(pub Arc<dyn crate::memory::service::MemoryService>);

#[async_trait]
impl AgentContextSource for MemoryContextSource {
    async fn collect(
        &self,
        scope: &AgentContextScope<'_>,
        _state: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        self.0.collect(scope).await
    }
}

pub struct TemporalContextSource {
    pub default_timezone: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct TemporalContextState {
    date: String,
    timezone: String,
    text: String,
}

#[async_trait]
impl AgentContextSource for TemporalContextSource {
    async fn collect(
        &self,
        scope: &AgentContextScope<'_>,
        state: &mut AgentContextState,
    ) -> Result<Vec<AgentContextSection>, AppError> {
        let timezone = scope
            .user
            .timezone
            .as_deref()
            .unwrap_or(&self.default_timezone);
        let now = chrono::Utc::now();
        let zone: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
        let local = now.with_timezone(&zone);
        let date = local.format("%Y-%m-%d").to_string();
        let mut cached = state.get::<TemporalContextState>()?;
        if cached.date != date || cached.timezone != timezone || cached.text.is_empty() {
            cached = TemporalContextState {
                date,
                timezone: timezone.to_owned(),
                text: format!(
                    "- current_date_local: {}\n- user_timezone: {}",
                    local.format("%Y-%m-%d (%A)"),
                    timezone,
                ),
            };
            state.set(cached.clone())?;
        }
        let mut sections = vec![AgentContextSection::prose(
            AgentContextKind::Temporal,
            "temporal_context",
            cached.text,
        )];
        if let Some(task) = scope.task {
            let fmt = "%Y-%m-%d %H:%M:%S %Z";
            let mut text = format!(
                "- created_at: {}\n",
                task.created_at.with_timezone(&zone).format(fmt)
            );
            if let Some(run_at) = task.run_at {
                text.push_str(&format!(
                    "- scheduled_at: {}\n",
                    run_at.with_timezone(&zone).format(fmt)
                ));
            }
            text.push_str(&format!("- now: {}", local.format(fmt)));
            let mut section =
                AgentContextSection::prose(AgentContextKind::TaskTime, "task_time", text);
            section.transient = true;
            sections.push(section);
        }
        Ok(sections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::tests::scope;

    #[tokio::test]
    async fn temporal_snapshot_refreshes_stale_date_and_changed_timezone() {
        let source = TemporalContextSource {
            default_timezone: "UTC".into(),
        };
        let mut user = scope().user.clone();
        user.timezone = Some("America/Los_Angeles".into());
        let scoped = AgentContextScope {
            user: &user,
            ..scope()
        };
        let mut state = AgentContextState::default();
        let first =
            AgentContextSnapshot::new(source.collect(&scoped, &mut state).await.unwrap()).unwrap();
        assert!(
            first
                .render()
                .contains("user_timezone: America/Los_Angeles")
        );
        let saved_state = state.clone();
        let same =
            AgentContextSnapshot::new(source.collect(&scoped, &mut state).await.unwrap()).unwrap();
        assert_eq!(same, first);
        assert_eq!(saved_state, state);

        // A persisted previous day must be replaced on the next collection.
        let mut stale = state.get::<TemporalContextState>().unwrap();
        stale.date = "2000-01-01".into();
        stale.text =
            "- current_date_local: 2000-01-01 (Saturday)\n- user_timezone: America/Los_Angeles"
                .into();
        state.set(stale).unwrap();
        let refreshed =
            AgentContextSnapshot::new(source.collect(&scoped, &mut state).await.unwrap()).unwrap();
        assert_eq!(refreshed, first);
        assert_eq!(state, saved_state);

        // With no user timezone, use the configured default.
        user.timezone = None;
        let scoped = AgentContextScope {
            user: &user,
            ..scope()
        };
        let changed_zone =
            AgentContextSnapshot::new(source.collect(&scoped, &mut state).await.unwrap()).unwrap();
        assert!(
            changed_zone
                .diff(&first)
                .unwrap()
                .contains("user_timezone: UTC")
        );
        let mut restored: AgentContextState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            source.collect(&scoped, &mut restored).await.unwrap(),
            changed_zone.sections
        );
    }

    #[tokio::test]
    async fn task_timestamp_changes_each_collection_without_changing_daily_snapshot() {
        let source = TemporalContextSource {
            default_timezone: "UTC".into(),
        };
        let mut state = AgentContextState::default();
        let task = serde_json::from_value(serde_json::json!({
            "id": "task", "user_id": "user", "agent_id": "agent", "title": "Task",
            "description": "Test", "status": "pending", "created_at": "2026-10-30T23:00:00Z",
            "updated_at": "2026-10-30T23:00:00Z", "run_at": "2026-10-30T23:00:00Z"
        }))
        .unwrap();
        let scoped = AgentContextScope {
            task: Some(&task),
            ..scope()
        };
        let first = source.collect(&scoped, &mut state).await.unwrap();
        // Task timestamps have second precision; collection must read the clock again.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let second = source.collect(&scoped, &mut state).await.unwrap();
        assert_eq!(first[0], second[0]);
        assert!(!first[0].transient);
        assert!(first[1].transient);
        assert!(
            first[1]
                .render()
                .contains("- created_at: 2026-10-30 23:00:00 UTC")
        );
        assert!(
            first[1]
                .render()
                .contains("- scheduled_at: 2026-10-30 23:00:00 UTC")
        );
        assert!(first[1].render().contains("- now: "));
        assert_ne!(first[1], second[1]);
        assert_eq!(
            AgentContextSnapshot::new(first).unwrap(),
            AgentContextSnapshot::new(second).unwrap()
        );
    }
}
