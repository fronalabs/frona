use surrealdb::Surreal;
use surrealdb::engine::local::{Db, RocksDb};
use surrealdb::types::RecordId;
use tracing::info;

use crate::agent::config::parse_frontmatter;
use crate::agent::workspace::AgentWorkspaceManager;

pub async fn setup_schema(db: &Surreal<Db>) -> Result<(), surrealdb::Error> {
    db.use_ns("frona").use_db("frona").await?;

    db.query(
        "
        DEFINE TABLE IF NOT EXISTS user SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS unique_email ON TABLE user COLUMNS email UNIQUE;

        DEFINE TABLE IF NOT EXISTS agent SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_agent_user ON TABLE agent COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS space SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_space_user ON TABLE space COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS chat SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_chat_user ON TABLE chat COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_chat_space ON TABLE chat COLUMNS space_id;

        DEFINE TABLE IF NOT EXISTS message SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_message_chat ON TABLE message COLUMNS chat_id;

        DEFINE TABLE IF NOT EXISTS task SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_task_user ON TABLE task COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_task_agent ON TABLE task COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_task_status ON TABLE task COLUMNS status;

        DEFINE TABLE IF NOT EXISTS credential SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_credential_user ON TABLE credential COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_credential_user_provider ON TABLE credential COLUMNS user_id, provider;

        DEFINE TABLE IF NOT EXISTS memory SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_memory_source ON TABLE memory COLUMNS source_type, source_id;

        DEFINE TABLE IF NOT EXISTS insight SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_insight_agent ON TABLE insight COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_insight_user ON TABLE insight COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS skill SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_skill_agent ON TABLE skill COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_skill_agent_name ON TABLE skill COLUMNS agent_id, name UNIQUE;

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_messages ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM message WHERE chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_task_chat ON TABLE task
          WHEN $event = 'DELETE' AND $before.chat_id IS NOT NONE
          THEN (DELETE type::record('chat', $before.chat_id));
        ",
    )
    .await?;

    Ok(())
}

pub async fn seed_config_agents(db: &Surreal<Db>, workspaces: &AgentWorkspaceManager) -> Result<(), surrealdb::Error> {
    let agent_ids: Vec<String> = workspaces.builtin_agent_ids().into_iter().map(String::from).collect();
    info!(agents = ?agent_ids, "Builtin agent IDs from config");
    for agent_id in agent_ids {
        let rid = RecordId::new("agent", agent_id.as_str());
        let mut result = db
            .query("SELECT meta::id(id) as id FROM agent WHERE id = $id LIMIT 1")
            .bind(("id", rid))
            .await?;

        let existing: Option<serde_json::Value> = result.take(0)?;
        if existing.is_some() {
            info!(agent_id = %agent_id, "Config agent already exists, skipping");
            continue;
        }

        let ws = workspaces.get(&agent_id);
        let (description, model_group) = ws
            .read("AGENT.md")
            .map(|content| {
                let entry = parse_frontmatter(&content);
                let desc = entry.metadata.get("description").cloned().unwrap_or_default();
                let mg = entry.metadata.get("model_group").cloned().unwrap_or_else(|| "primary".to_string());
                (desc, mg)
            })
            .unwrap_or_default();

        db.query(
            "CREATE type::record('agent', $id) SET
                name = $id,
                description = $description,
                model_group = $model_group,
                enabled = true,
                tools = [],
                identity = {},
                created_at = time::now(),
                updated_at = time::now()"
        )
        .bind(("id", agent_id.clone()))
        .bind(("description", description))
        .bind(("model_group", model_group))
        .await?;

        info!(agent_id = %agent_id, "Seeded config agent into database");
    }

    Ok(())
}

pub async fn init(path: &str) -> Result<Surreal<Db>, surrealdb::Error> {
    info!("Initializing SurrealDB at {path}");
    let db = Surreal::new::<RocksDb>(path).await?;

    setup_schema(&db).await?;

    info!("SurrealDB schema initialized");
    Ok(db)
}
