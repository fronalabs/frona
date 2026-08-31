use surrealdb::Surreal;
use surrealdb::engine::local::{Db, RocksDb};
use tracing::info;

/// Cascade-delete sources keyed by user_id. Order doesn't matter - none of
/// these reference each other, and chat fans out to message/tool_call/binding
/// through its own cascade events below.
const USER_OWNED_TABLES: &[(&str, &str)] = &[
    ("agent", "user_id"),
    ("space", "user_id"),
    ("chat", "user_id"),
    ("task", "user_id"),
    ("contact", "user_id"),
    ("credential", "user_id"),
    ("vault_connection", "user_id"),
    ("vault_grant", "user_id"),
    ("vault_access_log", "user_id"),
    ("principal_credential_binding", "user_id"),
    ("share", "user_id"),
    ("memory", "user_id"),
    ("keypair", "user_id"),
    ("notification", "user_id"),
    ("policy", "user_id"),
    ("oauth_identity", "user_id"),
    ("api_token", "user_id"),
    ("app", "user_id"),
    ("mcp_server", "user_id"),
    ("channel", "user_id"),
];

fn build_cascade_user_delete_events() -> String {
    let mut out = String::new();
    for (table, fk) in USER_OWNED_TABLES {
        out.push_str(&format!(
            "DEFINE EVENT IF NOT EXISTS cascade_delete_user_owned_{table} ON TABLE user
              WHEN $event = 'DELETE'
              THEN (DELETE FROM {table} WHERE {fk} = meta::id($before.id));\n"
        ));
    }
    out
}

pub async fn setup_schema(db: &Surreal<Db>) -> Result<(), surrealdb::Error> {
    db.use_ns("frona").use_db("frona").await?;

    let static_schema = "
        DEFINE TABLE IF NOT EXISTS user SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS unique_email ON TABLE user COLUMNS email UNIQUE;
        DEFINE INDEX IF NOT EXISTS unique_handle ON TABLE user COLUMNS handle UNIQUE;

        DEFINE TABLE IF NOT EXISTS user_group SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_user_group_name ON TABLE user_group COLUMNS name UNIQUE;

        DEFINE TABLE IF NOT EXISTS agent SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_agent_user ON TABLE agent COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_agent_user_handle ON TABLE agent COLUMNS user_id, handle UNIQUE;

        DEFINE TABLE IF NOT EXISTS space SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_space_user ON TABLE space COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS chat SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_chat_user ON TABLE chat COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_chat_space ON TABLE chat COLUMNS space_id;
        DEFINE INDEX IF NOT EXISTS idx_chat_channel ON TABLE chat COLUMNS channel_id;
        DEFINE INDEX IF NOT EXISTS idx_chat_channel_thread ON TABLE chat COLUMNS channel_id, channel_external_id UNIQUE;

        DEFINE TABLE IF NOT EXISTS message SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_message_chat ON TABLE message COLUMNS chat_id;
        DEFINE INDEX IF NOT EXISTS idx_message_delivery_due ON TABLE message COLUMNS delivery.state, delivery.next_attempt_at;

        DEFINE TABLE IF NOT EXISTS chat_summary SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_chat_summary_chat ON TABLE chat_summary COLUMNS chat_id UNIQUE;

        DEFINE TABLE IF NOT EXISTS task SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_task_user ON TABLE task COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_task_agent ON TABLE task COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_task_status ON TABLE task COLUMNS status;

        DEFINE TABLE IF NOT EXISTS credential SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_credential_user ON TABLE credential COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_credential_user_provider ON TABLE credential COLUMNS user_id, provider;

        DEFINE TABLE IF NOT EXISTS memory SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_memory_source ON TABLE memory COLUMNS source_type, source_id;

        DEFINE TABLE IF NOT EXISTS memory_entry SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_memory_entry_agent ON TABLE memory_entry COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_memory_entry_user ON TABLE memory_entry COLUMNS user_id;

        DEFINE ANALYZER IF NOT EXISTS knowledge_search TOKENIZERS class FILTERS lowercase, ascii;
        DEFINE TABLE IF NOT EXISTS knowledge_entity SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_user ON TABLE knowledge_entity COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_user_path ON TABLE knowledge_entity COLUMNS user_id, path UNIQUE;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_search ON TABLE knowledge_entity FIELDS search_text FULLTEXT ANALYZER knowledge_search BM25;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_body_search ON TABLE knowledge_entity FIELDS body FULLTEXT ANALYZER knowledge_search BM25;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_names ON TABLE knowledge_entity COLUMNS user_id, search_names.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_name_tokens ON TABLE knowledge_entity COLUMNS user_id, search_name_tokens.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_search_assertions ON TABLE knowledge_entity COLUMNS user_id, search_assertions.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_resolution_kinds ON TABLE knowledge_entity COLUMNS user_id, kinds.*;
        DEFINE TABLE IF NOT EXISTS knowledge_memory SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_memory_user ON TABLE knowledge_memory COLUMNS user_id;
        DEFINE TABLE IF NOT EXISTS knowledge_entity_source SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_source_user ON TABLE knowledge_entity_source COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_source_path ON TABLE knowledge_entity_source COLUMNS user_id, entity_path;
        DEFINE TABLE IF NOT EXISTS knowledge_entity_link SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_link_from ON TABLE knowledge_entity_link COLUMNS user_id, from_entity_path;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_link_origin ON TABLE knowledge_entity_link COLUMNS user_id, origin;
        DEFINE TABLE IF NOT EXISTS knowledge_short_memory SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_short_memory_user ON TABLE knowledge_short_memory COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_short_memory_chat ON TABLE knowledge_short_memory COLUMNS source_chat_id;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_entity_origin ON TABLE knowledge_entity COLUMNS user_id, origin;
        DEFINE TABLE IF NOT EXISTS knowledge_ontology SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_ontology_user ON TABLE knowledge_ontology COLUMNS user_id UNIQUE;

        -- The two consolidation tables. Per CHAT: how far the transcript has been mined,
        -- so a chat is never re-read. Per PASS: where the pipeline got to, so a failure
        -- resumes rather than starting over.
        DEFINE TABLE IF NOT EXISTS knowledge_consolidation_watermark SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_watermark_chat ON TABLE knowledge_consolidation_watermark COLUMNS chat_id UNIQUE;
        -- NOT UNIQUE on user_id: finished passes are kept as a log, and the live pass is
        -- the newest row (ids are UUIDv7, so time-ordered).
        DEFINE TABLE IF NOT EXISTS knowledge_consolidation_record SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_record_user ON TABLE knowledge_consolidation_record COLUMNS user_id;
        -- Durable, run-scoped working pages. These rows form the effective overlay used
        -- by every consolidation read and are removed only by terminal cleanup.
        DEFINE TABLE IF NOT EXISTS knowledge_consolidation_entity SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_identity ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, path UNIQUE;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_search_scope ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, searchable, category;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_lifecycle ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, lifecycle;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_search ON TABLE knowledge_consolidation_entity FIELDS search_text FULLTEXT ANALYZER knowledge_search BM25;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_names ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, search_names.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_name_tokens ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, search_name_tokens.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_search_assertions ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, search_assertions.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_resolution_kinds ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, kinds.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_target ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, outgoing_links.*.target_path;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_memory ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, source_memory_ids.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_attribute_memory ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, attribute_sources.*.source_memory_ids.*;
        DEFINE INDEX IF NOT EXISTS idx_knowledge_consolidation_entity_link_memory ON TABLE knowledge_consolidation_entity COLUMNS consolidation_id, outgoing_links.*.source_memory_ids.*;
        DEFINE EVENT IF NOT EXISTS cascade_delete_knowledge_consolidation_entities ON TABLE knowledge_consolidation_record
          WHEN $event = 'DELETE' OR ($event = 'UPDATE' AND $before.consolidation_id != $after.consolidation_id)
          THEN (DELETE FROM knowledge_consolidation_entity WHERE consolidation_id = $before.consolidation_id);

        DEFINE TABLE IF NOT EXISTS user_config SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_user_config_user ON TABLE user_config COLUMNS user_id UNIQUE;

        DEFINE TABLE IF NOT EXISTS keypair SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_keypair_owner ON TABLE keypair COLUMNS owner UNIQUE;

        DEFINE TABLE IF NOT EXISTS api_token SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_api_token_user ON TABLE api_token COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_api_token_pair ON TABLE api_token COLUMNS refresh_pair_id;

        DEFINE TABLE IF NOT EXISTS contact SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_contact_user ON TABLE contact COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_contact_phone ON TABLE contact COLUMNS user_id, phone;
        DEFINE INDEX IF NOT EXISTS idx_contact_addresses ON TABLE contact COLUMNS user_id, addresses.provider, addresses.address UNIQUE;

        DEFINE TABLE IF NOT EXISTS oauth_identity SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_oauth_identity_sub ON TABLE oauth_identity COLUMNS external_sub UNIQUE;
        DEFINE INDEX IF NOT EXISTS idx_oauth_identity_user ON TABLE oauth_identity COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS notification SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_notification_user ON TABLE notification COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS policy SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_policy_user ON TABLE policy COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_policy_user_name ON TABLE policy COLUMNS user_id, name UNIQUE;

        DEFINE TABLE IF NOT EXISTS call SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_call_chat ON TABLE call COLUMNS chat UNIQUE;

        DEFINE TABLE IF NOT EXISTS inference_usage SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_iu_user_created ON TABLE inference_usage COLUMNS user_id, created_at;
        DEFINE INDEX IF NOT EXISTS idx_iu_chat_created ON TABLE inference_usage COLUMNS chat_id, created_at;
        DEFINE INDEX IF NOT EXISTS idx_iu_agent_created ON TABLE inference_usage COLUMNS agent_id, created_at;
        DEFINE INDEX IF NOT EXISTS idx_iu_space_created ON TABLE inference_usage COLUMNS space_id, created_at;
        DEFINE INDEX IF NOT EXISTS idx_iu_kind_created ON TABLE inference_usage COLUMNS kind_tag, created_at;
        DEFINE INDEX IF NOT EXISTS idx_iu_model_created ON TABLE inference_usage COLUMNS model_ref, created_at;
        DEFINE INDEX IF NOT EXISTS idx_iu_pricing_version ON TABLE inference_usage COLUMNS pricing_version;

        DEFINE TABLE IF NOT EXISTS app SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_app_agent ON TABLE app COLUMNS agent_id;
        DEFINE INDEX IF NOT EXISTS idx_app_user ON TABLE app COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_app_status ON TABLE app COLUMNS status;
        DEFINE INDEX IF NOT EXISTS idx_app_user_handle ON TABLE app COLUMNS user_id, handle UNIQUE;

        DEFINE TABLE IF NOT EXISTS mcp_server SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_mcp_server_user ON TABLE mcp_server COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_mcp_server_status ON TABLE mcp_server COLUMNS status;
        DEFINE INDEX IF NOT EXISTS idx_mcp_server_user_handle ON TABLE mcp_server COLUMNS user_id, handle UNIQUE;

        DEFINE TABLE IF NOT EXISTS channel SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_channel_user ON TABLE channel COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_channel_space ON TABLE channel COLUMNS space_id;
        DEFINE INDEX IF NOT EXISTS idx_channel_status ON TABLE channel COLUMNS status;
        DEFINE INDEX IF NOT EXISTS idx_channel_space_unique ON TABLE channel COLUMNS space_id UNIQUE;
        DEFINE INDEX IF NOT EXISTS idx_channel_user_handle ON TABLE channel COLUMNS user_id, handle UNIQUE;

        DEFINE TABLE IF NOT EXISTS vault_connection SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_vault_connection_user ON TABLE vault_connection COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS vault_grant SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_vault_grant_user ON TABLE vault_grant COLUMNS user_id;
        DEFINE INDEX IF NOT EXISTS idx_vault_grant_user_principal ON TABLE vault_grant COLUMNS user_id, principal.kind, principal.id;

        DEFINE TABLE IF NOT EXISTS principal_credential_binding SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_pcb_user_principal ON TABLE principal_credential_binding COLUMNS user_id, principal.kind, principal.id;
        DEFINE INDEX IF NOT EXISTS idx_pcb_lookup ON TABLE principal_credential_binding COLUMNS user_id, principal.kind, principal.id, query;
        DEFINE INDEX IF NOT EXISTS idx_pcb_chat ON TABLE principal_credential_binding COLUMNS scope.Chat.chat_id;

        DEFINE TABLE IF NOT EXISTS tool_call SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_tool_call_chat ON TABLE tool_call COLUMNS chat_id;
        DEFINE INDEX IF NOT EXISTS idx_tool_call_message ON TABLE tool_call COLUMNS message_id;

        DEFINE TABLE IF NOT EXISTS vault_access_log SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_vault_access_log_chat ON TABLE vault_access_log COLUMNS chat_id;
        DEFINE INDEX IF NOT EXISTS idx_vault_access_log_user ON TABLE vault_access_log COLUMNS user_id;

        DEFINE TABLE IF NOT EXISTS share SCHEMALESS;
        DEFINE INDEX IF NOT EXISTS idx_share_expires ON TABLE share COLUMNS expires_at;

        DEFINE TABLE IF NOT EXISTS runtime_config SCHEMAFULL;
        DEFINE FIELD IF NOT EXISTS `key` ON runtime_config TYPE string;
        DEFINE FIELD IF NOT EXISTS `value` ON runtime_config TYPE string;
        DEFINE FIELD IF NOT EXISTS updated_at ON runtime_config TYPE datetime;
        DEFINE INDEX IF NOT EXISTS idx_runtime_config_key ON runtime_config FIELDS `key` UNIQUE;

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_messages ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM message WHERE chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_bindings ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM principal_credential_binding
                  WHERE scope.Chat.chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_tool_calls ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM tool_call WHERE chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_vault_access_logs ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM vault_access_log WHERE chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_calls ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM call WHERE chat = $before.id);

        DEFINE EVENT IF NOT EXISTS cascade_delete_chat_summary ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM chat_summary WHERE chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_knowledge_consolidation_watermark ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM knowledge_consolidation_watermark WHERE chat_id = meta::id($before.id));

        DEFINE EVENT IF NOT EXISTS cascade_delete_task_chat ON TABLE task
          WHEN $event = 'DELETE' AND $before.chat_id IS NOT NONE
          THEN (DELETE type::record('chat', $before.chat_id));

        DEFINE EVENT IF NOT EXISTS refuse_last_admin_loss_on_delete ON TABLE user
          WHEN $event = 'DELETE'
            AND $before.deactivated_at IS NONE
            AND $before.groups CONTAINS 'admins'
          THEN {
            LET $remaining = (SELECT count() FROM user
                                WHERE deactivated_at IS NONE
                                  AND groups CONTAINS 'admins'
                                GROUP ALL)[0].count ?? 0;
            IF $remaining < 1 { THROW 'last_admin'; };
          };

        DEFINE EVENT IF NOT EXISTS refuse_last_admin_loss_on_update ON TABLE user
          WHEN $event = 'UPDATE'
            AND $before.deactivated_at IS NONE
            AND $before.groups CONTAINS 'admins'
            AND ($after.deactivated_at IS NOT NONE
              OR !($after.groups CONTAINS 'admins'))
          THEN {
            LET $remaining = (SELECT count() FROM user
                                WHERE deactivated_at IS NONE
                                  AND groups CONTAINS 'admins'
                                GROUP ALL)[0].count ?? 0;
            IF $remaining < 1 { THROW 'last_admin'; };
          };
        ";

    let cascade_events = build_cascade_user_delete_events();
    let schema = format!("{static_schema}\n{cascade_events}");

    db.query(schema).await?;

    Ok(())
}

pub async fn init(path: &str) -> Result<Surreal<Db>, surrealdb::Error> {
    info!("Initializing SurrealDB at {path}");

    let timeout = std::time::Duration::from_secs(60);
    let interval = std::time::Duration::from_secs(2);
    let start = std::time::Instant::now();

    let db = loop {
        match Surreal::new::<RocksDb>(path).await {
            Ok(db) => break db,
            Err(e) => {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    tracing::error!("Failed to open database after {elapsed:.0?}: {e}");
                    std::process::exit(1);
                }
                tracing::warn!("Database locked, retrying ({:.0?} elapsed): {e}", elapsed);
                tokio::time::sleep(interval).await;
            }
        }
    };

    setup_schema(&db).await?;
    crate::db::migrations::run_migrations(&db).await?;

    info!("SurrealDB schema initialized");
    Ok(db)
}
