DEFINE TABLE IF NOT EXISTS user SCHEMALESS;

DEFINE INDEX IF NOT EXISTS unique_email ON TABLE user COLUMNS email UNIQUE;

DEFINE INDEX IF NOT EXISTS unique_handle ON TABLE user COLUMNS handle UNIQUE;

DEFINE TABLE IF NOT EXISTS user_group SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_user_group_name ON TABLE user_group COLUMNS name UNIQUE;

DEFINE TABLE IF NOT EXISTS agent SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_agent_user ON TABLE agent COLUMNS user_id;

DEFINE INDEX IF NOT EXISTS idx_agent_user_handle ON TABLE agent COLUMNS user_id, handle UNIQUE;

DEFINE TABLE IF NOT EXISTS chat SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_chat_user ON TABLE chat COLUMNS user_id;

DEFINE INDEX IF NOT EXISTS idx_chat_space ON TABLE chat COLUMNS space_id;

DEFINE INDEX IF NOT EXISTS idx_chat_channel ON TABLE chat COLUMNS channel_id;

DEFINE INDEX IF NOT EXISTS idx_chat_channel_thread ON TABLE chat COLUMNS channel_id, channel_external_id UNIQUE;

DEFINE TABLE IF NOT EXISTS message SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_message_chat ON TABLE message COLUMNS chat_id;

DEFINE INDEX IF NOT EXISTS idx_message_chat_order ON TABLE message COLUMNS chat_id, created_at, id;

DEFINE INDEX IF NOT EXISTS idx_message_context_signature ON TABLE message COLUMNS chat_id, agent_context_signature;

DEFINE INDEX IF NOT EXISTS idx_message_delivery_due ON TABLE message COLUMNS delivery.state, delivery.next_attempt_at;

DEFINE TABLE IF NOT EXISTS chat_summary SCHEMALESS;

DEFINE TABLE IF NOT EXISTS agent_context_snapshot SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_chat_summary_chat ON TABLE chat_summary COLUMNS chat_id UNIQUE;

DEFINE TABLE IF NOT EXISTS task SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_task_user ON TABLE task COLUMNS user_id;

DEFINE INDEX IF NOT EXISTS idx_task_agent ON TABLE task COLUMNS agent_id;

DEFINE INDEX IF NOT EXISTS idx_task_status ON TABLE task COLUMNS status;

DEFINE TABLE IF NOT EXISTS user_config SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_user_config_user ON TABLE user_config COLUMNS user_id UNIQUE;

DEFINE TABLE IF NOT EXISTS policy SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_policy_user ON TABLE policy COLUMNS user_id;

DEFINE INDEX IF NOT EXISTS idx_policy_user_name ON TABLE policy COLUMNS user_id, name UNIQUE;

DEFINE TABLE IF NOT EXISTS inference_usage SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_iu_user_created ON TABLE inference_usage COLUMNS user_id, created_at;

DEFINE INDEX IF NOT EXISTS idx_iu_chat_created ON TABLE inference_usage COLUMNS chat_id, created_at;

DEFINE INDEX IF NOT EXISTS idx_iu_agent_created ON TABLE inference_usage COLUMNS agent_id, created_at;

DEFINE INDEX IF NOT EXISTS idx_iu_space_created ON TABLE inference_usage COLUMNS space_id, created_at;

DEFINE INDEX IF NOT EXISTS idx_iu_kind_created ON TABLE inference_usage COLUMNS kind_tag, created_at;

DEFINE INDEX IF NOT EXISTS idx_iu_model_created ON TABLE inference_usage COLUMNS model_ref, created_at;

DEFINE INDEX IF NOT EXISTS idx_iu_pricing_version ON TABLE inference_usage COLUMNS pricing_version;

DEFINE TABLE IF NOT EXISTS tool_call SCHEMALESS;

DEFINE INDEX IF NOT EXISTS idx_tool_call_chat ON TABLE tool_call COLUMNS chat_id;

DEFINE INDEX IF NOT EXISTS idx_tool_call_message ON TABLE tool_call COLUMNS message_id;

DEFINE EVENT IF NOT EXISTS cascade_delete_chat_messages ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM message WHERE chat_id = meta::id($before.id));

DEFINE EVENT IF NOT EXISTS cascade_delete_chat_tool_calls ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM tool_call WHERE chat_id = meta::id($before.id));

DEFINE EVENT IF NOT EXISTS cascade_delete_chat_summary ON TABLE chat
          WHEN $event = 'DELETE'
          THEN (DELETE FROM chat_summary WHERE chat_id = meta::id($before.id));

DEFINE EVENT IF NOT EXISTS cascade_delete_task_chat ON TABLE task
          WHEN $event = 'DELETE' AND $before.chat_id IS NOT NONE
          THEN (DELETE type::record('chat', $before.chat_id));
