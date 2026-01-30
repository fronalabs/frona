use std::sync::Arc;
use std::time::Duration;

use crate::api::repo::chats::SurrealChatRepo;
use crate::api::repo::facts::SurrealFactRepo;
use crate::api::repo::spaces::SurrealSpaceRepo;
use crate::chat::repository::ChatRepository;
use crate::error::AppError;
use crate::llm::config::ModelGroup;
use crate::memory::fact::repository::FactRepository;
use crate::memory::models::MemorySourceType;
use crate::memory::service::MemoryService;
use crate::space::repository::SpaceRepository;

pub struct Scheduler {
    memory_service: MemoryService,
    space_repo: SurrealSpaceRepo,
    chat_repo: SurrealChatRepo,
    fact_repo: SurrealFactRepo,
    compaction_model_group: ModelGroup,
    interval: Duration,
}

impl Scheduler {
    pub fn new(
        memory_service: MemoryService,
        space_repo: SurrealSpaceRepo,
        chat_repo: SurrealChatRepo,
        fact_repo: SurrealFactRepo,
        compaction_model_group: ModelGroup,
        interval: Duration,
    ) -> Self {
        Self {
            memory_service,
            space_repo,
            chat_repo,
            fact_repo,
            compaction_model_group,
            interval,
        }
    }

    pub fn start(self: Arc<Self>) {
        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(scheduler.interval).await;
                if let Err(e) = scheduler.run_space_compaction().await {
                    tracing::warn!(error = %e, "Scheduled space compaction failed");
                }
            }
        });

        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(7200)).await;
                if let Err(e) = scheduler.run_fact_compaction().await {
                    tracing::warn!(error = %e, "Scheduled fact compaction failed");
                }
            }
        });
    }

    async fn run_space_compaction(&self) -> Result<(), AppError> {
        let spaces = self.space_repo.find_all().await?;

        for space in spaces {
            let chats = self.chat_repo.find_by_space_id(&space.id).await?;
            if chats.is_empty() {
                continue;
            }

            let mut summaries = Vec::new();
            for chat in &chats {
                let title = chat.title.clone().unwrap_or_else(|| "Untitled".to_string());

                let memory = self
                    .memory_service
                    .get_memory(MemorySourceType::Chat, &chat.id)
                    .await?;

                let summary = if let Some(mem) = memory {
                    mem.content
                } else {
                    format!("(No summary available for chat: {title})")
                };

                summaries.push((title, summary));
            }

            if let Err(e) = self
                .memory_service
                .compact_space(&space.id, summaries, &self.compaction_model_group)
                .await
            {
                tracing::warn!(
                    space_id = %space.id,
                    error = %e,
                    "Failed to compact space"
                );
            }
        }

        Ok(())
    }

    async fn run_fact_compaction(&self) -> Result<(), AppError> {
        let agent_ids = self.fact_repo.find_distinct_agent_ids().await?;

        tracing::info!(
            agent_count = agent_ids.len(),
            "Starting scheduled fact compaction"
        );

        for agent_id in &agent_ids {
            tracing::info!(agent_id = %agent_id, "Running scheduled fact compaction for agent");
            if let Err(e) = self
                .memory_service
                .compact_facts_if_needed(agent_id, &self.compaction_model_group)
                .await
            {
                tracing::warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "Failed to compact facts for agent"
                );
            }
        }

        Ok(())
    }
}
