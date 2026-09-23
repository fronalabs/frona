import { api } from "./api-client";

// Mirrors Rust Config struct - sensitive fields come as { is_set: boolean } from GET
export type SensitiveField = string | { is_set: boolean };

export interface ServerConfig {
  port: number;
  static_dir: string;
  issuer_url: string;
  max_concurrent_tasks: number;
  cors_origins: string | null;
  base_url: string | null;
  backend_url: string | null;
  frontend_url: string | null;
  external_url: string | null;
  max_body_size_bytes: number;
  /** Default IANA timezone for users with no profile timezone set.
      Empty string → auto-detect from TZ env var / /etc/localtime / fall back to UTC. */
  timezone: string;
}

export interface SandboxConfig {
  disabled: boolean;
  /** Flattened from `default_limits: SandboxLimits` server-side, so the
      JSON shape stays one level deep. */
  max_cpu_pct: number;
  max_memory_pct: number;
  timeout_secs: number;
  max_total_cpu_pct: number;
  max_total_memory_pct: number;
  default_network_access: boolean;
}

export interface AuthConfig {
  encryption_secret: SensitiveField;
  access_token_expiry_secs: number;
  refresh_token_expiry_secs: number;
  presign_expiry_secs: number;
  allow_registration: boolean;
}

export interface SsoConfig {
  enabled: boolean;
  authority: string | null;
  client_id: string | null;
  client_secret: SensitiveField;
  scopes: string;
  allow_unknown_email_verification: boolean;
  client_cache_expiration: number;
  disable_local_auth: boolean;
  signups_match_email: boolean;
}

export interface BrowserConfig {
  ws_url: string;
  profiles_path: string;
  connection_timeout_ms: number;
}

export interface SearchConfig {
  provider: string | null;
  searxng_base_url: string | null;
}

export interface VoiceConfig {
  provider: string | null;
  twilio_account_sid: SensitiveField;
  twilio_auth_token: SensitiveField;
  twilio_from_number: string | null;
  twilio_voice_id: string | null;
  twilio_speech_model: string | null;
}

export interface VaultConfig {
  onepassword_service_account_token: SensitiveField;
  onepassword_vault_id: string | null;
  bitwarden_client_id: string | null;
  bitwarden_client_secret: SensitiveField;
  bitwarden_master_password: SensitiveField;
  bitwarden_server_url: string | null;
  hashicorp_address: string | null;
  hashicorp_token: SensitiveField;
  hashicorp_mount: string | null;
  keepass_path: string | null;
  keepass_password: SensitiveField;
}

export interface RetryConfig {
  max_retries: number;
  initial_backoff_ms: number;
  backoff_multiplier: number;
  max_backoff_ms: number;
}

export interface AnthropicThinking {
  type: string;
  budget_tokens?: number | null;
}

export interface GeminiThinkingConfig {
  thinking_budget: number;
  include_thoughts?: boolean | null;
}

export interface ModelGroupConfig {
  provider: string;
  model: string;
  api?: import("./provider-admin").ProviderProtocol;
  extra_params?: Record<string, unknown>;
  fallbacks?: ModelGroupConfig[];
  max_tokens?: number | null;
  temperature?: number | null;
  context_window?: number | null;
  retry?: RetryConfig;
  thinking?: AnthropicThinking | null;
  top_p?: number | null;
  top_k?: number | null;
  stop_sequences?: string[] | null;
  think?: boolean | null;
  num_ctx?: number | null;
  num_predict?: number | null;
  num_batch?: number | null;
  num_keep?: number | null;
  num_thread?: number | null;
  num_gpu?: number | null;
  min_p?: number | null;
  repeat_penalty?: number | null;
  repeat_last_n?: number | null;
  frequency_penalty?: number | null;
  presence_penalty?: number | null;
  mirostat?: number | null;
  mirostat_eta?: number | null;
  mirostat_tau?: number | null;
  tfs_z?: number | null;
  seed?: number | null;
  stop?: string[] | null;
  use_mmap?: boolean | null;
  use_mlock?: boolean | null;
  max_completion_tokens?: number | null;
  reasoning_effort?: string | null;
  logprobs?: boolean | null;
  top_logprobs?: number | null;
  thinking_config?: GeminiThinkingConfig | null;
  candidate_count?: number | null;
  [key: string]: unknown;
}

export interface ModelProviderConfig {
  credential_id?: string | null;
  provider?: string;
  adapter?: string;
  api_key: SensitiveField | null;
  base_url: string | null;
  enabled: boolean;
  [key: string]: unknown;
}

export interface InferenceConfig {
  max_tool_turns: number;
  default_max_tokens: number;
  compaction_trigger_pct: number;
  history_truncation_pct: number;
}

export interface SchedulerConfig {
  poll_secs: number;
}

export interface AppConfig {
  port_range_start: number;
  port_range_end: number;
  health_check_timeout_secs: number;
  max_restart_attempts: number;
  hibernate_after_secs: number;
}

export type MemoryBackend = "basic" | "pkm";

export interface MemoryConfig {
  /** `null` = unconfigured; the server resolves it at boot (PKM for a fresh install,
   *  Basic for an existing one). The UI renders `null` as a concrete selection. */
  backend: MemoryBackend | null;
  model_group: string;
  basic_compaction_token_threshold: number;
  basic_compaction_secs: number;
  basic_space_compaction_secs: number;
  pkm_search_top_k: number;
  pkm_short_memory_half_life_secs: number;
  pkm_short_memory_demote_threshold: number;
  pkm_short_memory_top_n: number;
  pkm_short_memory_token_cap: number;
  pkm_playbook_index_token_cap: number;
  pkm_consolidate_secs: number;
  pkm_consolidate_idle_secs: number;
  pkm_consolidation_concurrency: number;
  pkm_consolidation_max_tool_turns: number;
  pkm_consolidation_max_submissions: number;
  pkm_playbook_max_tool_turns: number;
  pkm_playbook_max_submissions: number;
  pkm_extract_max_tokens: number;
  pkm_extract_max_messages: number;
  pkm_extract_agent_evidence_lookback_messages: number;
  pkm_extract_agent_evidence_result_token_cap: number;
  pkm_consolidation_max_attempts: number;
  pkm_adjudication_max_attempts_per_batch: number;
  pkm_consolidation_checkpoint_failure_cap: number;
  pkm_consolidation_retry_base_secs: number;
}

export interface Config {
  server: ServerConfig;
  sandbox: SandboxConfig;
  auth: AuthConfig;
  sso: SsoConfig;
  browser: BrowserConfig | null;
  search: SearchConfig;
  voice: VoiceConfig;
  vault: VaultConfig;
  inference: InferenceConfig;
  scheduler: SchedulerConfig;
  app: AppConfig;
  memory: MemoryConfig;
  models: Record<string, ModelGroupConfig>;
  providers: Record<string, ModelProviderConfig>;
}

export interface JsonSchemaProperty {
  type?: string;
  description?: string;
  default?: unknown;
  enum?: string[];
  "x-sensitive"?: boolean;
  properties?: Record<string, JsonSchemaProperty>;
  $ref?: string;
}

export interface JsonSchema {
  properties?: Record<string, JsonSchemaProperty>;
  definitions?: Record<string, JsonSchemaProperty>;
  $defs?: Record<string, JsonSchemaProperty>;
  $ref?: string;
}

export interface ConfigUpdateResponse {
  config: Config;
  authoring_document: Record<string, unknown>;
  persisted_revision: string;
  active_revision: string;
  restart_required: boolean;
  parameter_overrides: Array<{ config_path: string; wire_path: string[] }>;
}

export function getConfigSchema(): Promise<JsonSchema> {
  return api.get<JsonSchema>("/api/config/schema");
}

export async function getConfig(): Promise<Config> {
  const result = await getConfigDocument();
  return result.config;
}

export function getConfigDocument(): Promise<ConfigUpdateResponse> {
  return api.get<ConfigUpdateResponse>("/api/config");
}

function stripRedactedSensitiveFields(obj: unknown, path: string[] = []): unknown {
  if (obj === null || obj === undefined) return obj;
  if (typeof obj !== "object") return obj;
  if (Array.isArray(obj)) return obj.map((value, index) => stripRedactedSensitiveFields(value, [...path, String(index)]));
  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj as Record<string, unknown>)) {
    const nextPath = [...path, key];
    const sensitive = (nextPath.length === 3 && nextPath[0] === "providers" && key === "api_key")
      || (nextPath.length === 2 && [
        ["auth", "encryption_secret"], ["sso", "client_secret"],
        ["voice", "twilio_account_sid"], ["voice", "twilio_auth_token"],
        ["vault", "onepassword_service_account_token"], ["vault", "bitwarden_client_secret"],
        ["vault", "bitwarden_master_password"], ["vault", "hashicorp_token"], ["vault", "keepass_password"],
      ].some(([section, field]) => nextPath[0] === section && key === field));
    if (sensitive && typeof value === "object" && value !== null && "is_set" in value
      && Object.keys(value).length === 1 && typeof value.is_set === "boolean") continue;
    result[key] = stripRedactedSensitiveFields(value, nextPath);
  }
  return result;
}

export function updateConfig(
  patch: Record<string, unknown>,
  metadata?: { expectedPersistedRevision: string; baseline?: Config | null },
): Promise<ConfigUpdateResponse> {
  const cleaned = stripRedactedSensitiveFields(patch) as Record<string, unknown>;
  const changes = metadata?.baseline ? changedConfigFields(metadata.baseline, cleaned) : cleaned;
  return api.put<ConfigUpdateResponse>("/api/config", metadata ? {
    patch: changes,
    expected_persisted_revision: metadata.expectedPersistedRevision,
  } : changes);
}

/** Section editors return full values; only send fields changed from the loaded view.
 * Arrays and model extra_params remain complete replacements, including {} clears.
 * Missing patch keys are untouched, while explicit nulls retain deletion semantics.
 */
function changedConfigFields(before: object, patch: Record<string, unknown>, path: string[] = []): Record<string, unknown> {
  const baseline = before as Record<string, unknown>;
  const changes: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(patch)) {
    if (JSON.stringify(baseline[key]) === JSON.stringify(value)) continue;
    const previous = baseline[key];
    if (previous && value && typeof previous === "object" && typeof value === "object"
      && !Array.isArray(previous) && !Array.isArray(value)
      && !(path[0] === "models" && key === "extra_params")) {
      const nested = changedConfigFields(previous, value as Record<string, unknown>, [...path, key]);
      if (Object.keys(nested).length) changes[key] = nested;
    } else {
      changes[key] = value;
    }
  }
  return changes;
}

export interface ModelInfo {
  id: string;
  name?: string;
  context_window?: number;
  max_tokens?: number;
}

export async function getProviderModels(
  providerId: string,
  opts?: { apiKey?: string; baseUrl?: string }
): Promise<{ models: ModelInfo[] }> {
  const { providerAdmin } = await import("./provider-admin");
  if (opts?.apiKey) {
    const config = { provider: providerId, base_url: opts.baseUrl };
    const proof = await providerAdmin.validate(providerId, config, { source: "api_key", api_key: opts.apiKey });
    const models = proof.models ?? (await providerAdmin.draftModels(providerId, {
      config, validation_id: proof.validation_id, method: "api_key", source: "database",
    })).models;
    return { models: models.map(model => ({ id: model.id, name: model.name ?? undefined, context_window: model.context_window ?? undefined, max_tokens: model.max_tokens ?? undefined })) };
  }
  const result = await providerAdmin.models(providerId);
  return { models: result.models.map(model => ({ id: model.id, name: model.name ?? undefined, context_window: model.context_window ?? undefined, max_tokens: model.max_tokens ?? undefined })) };
}

export function isSensitiveSet(value: SensitiveField): boolean {
  if (typeof value === "object" && value !== null && "is_set" in value) {
    return value.is_set;
  }
  return typeof value === "string" && value.length > 0;
}
