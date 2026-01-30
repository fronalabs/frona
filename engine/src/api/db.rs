use surrealdb::Surreal;
use surrealdb::engine::local::{Db, RocksDb};
use surrealdb::types::RecordId;
use tracing::info;

use crate::agent::config::defaults::embedded_agent_ids;

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

        DEFINE TABLE IF NOT EXISTS prompt SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_prompt_agent ON TABLE prompt COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_prompt_agent_name ON TABLE prompt COLUMNS agent_id, name UNIQUE;

        DEFINE TABLE IF NOT EXISTS credential SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_credential_user ON TABLE credential COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_credential_user_provider ON TABLE credential COLUMNS user_id, provider;

        DEFINE TABLE IF NOT EXISTS memory SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_memory_source ON TABLE memory COLUMNS source_type, source_id;

        DEFINE TABLE IF NOT EXISTS fact SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_fact_agent ON TABLE fact COLUMNS agent_id;

        DEFINE TABLE IF NOT EXISTS skill SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_skill_agent ON TABLE skill COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_skill_agent_name ON TABLE skill COLUMNS agent_id, name UNIQUE;
        ",
    )
    .await?;

    Ok(())
}

pub async fn seed_config_agents(db: &Surreal<Db>) -> Result<(), surrealdb::Error> {
    for agent_id in embedded_agent_ids() {
        let rid = RecordId::new("agent", agent_id);
        let mut result = db
            .query("SELECT meta::id(id) as id FROM agent WHERE id = $id LIMIT 1")
            .bind(("id", rid))
            .await?;

        let existing: Option<serde_json::Value> = result.take(0)?;
        if existing.is_some() {
            continue;
        }

        db.query(
            "CREATE type::record('agent', $id) SET
                name = $id,
                description = '',
                system_prompt = '',
                model_group = 'primary',
                enabled = true,
                tools = [],
                created_at = time::now(),
                updated_at = time::now()"
        )
        .bind(("id", agent_id))
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
