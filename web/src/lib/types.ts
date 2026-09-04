export interface UserPermissions {
  list_users: boolean;
  is_admin: boolean;
}

export interface UserInfo {
  id: string;
  /** Rendered as "Username" in UI copy. */
  handle: string;
  email: string;
  name: string;
  timezone?: string;
  needs_setup?: boolean;
  permissions?: UserPermissions;
}

export interface AuthResponse {
  token?: string;
  user: UserInfo;
}

export interface LoginRequest {
  identifier: string;
  password: string;
}

export interface RegisterRequest {
  handle: string;
  email: string;
  name: string;
  password: string;
}

export interface SsoStatus {
  enabled: boolean;
  disable_local_auth: boolean;
}

export interface AuthConfig {
  sso: SsoStatus;
  allow_registration: boolean;
}

export interface SandboxPolicy {
  read_paths?: string[];
  write_paths?: string[];
  network_access?: boolean;
  network_destinations?: string[];
  bind_ports?: number[];
  denied_paths?: string[];
  blocked_networks?: string[];
}

export interface SandboxLimits {
  max_cpu_pct: number;
  max_memory_pct: number;
  timeout_secs: number;
}

export interface Agent {
  id: string;
  handle: string;
  name: string;
  description: string;
  model_group: string;
  /** Resolved view of the active model_group — what's actually being used at
   *  request time. Absent when the group name doesn't resolve. */
  model?: Model;
  enabled: boolean;
  tools: string[];
  skills: string[];
  avatar_url: string | null;
  identity: Record<string, string>;
  sandbox_policy: SandboxPolicy;
  sandbox_limits: SandboxLimits | null;
  prompt: string | null;
  default_prompt: string;
  is_shared: boolean;
  chat_count: number;
  created_at: string;
  updated_at: string;
}

/** Mirrors models.dev's flattened catalog entry plus the provider/model_id
 *  identity. Only the fields the UI currently consumes are typed here; add
 *  more as needed. */
export interface Model {
  provider: string;
  model_id: string;
  /** Resolved per-group context window: config override, else catalog
   *  max_input_tokens, else default. Authoritative for saturation displays. */
  context_window: number;
  limit?: { context: number; output: number; input?: number };
  cost?: {
    input: number;
    output: number;
    cache_read?: number;
    cache_write?: number;
  };
  attachment?: boolean;
  reasoning?: boolean;
  tool_call?: boolean;
  structured_output?: boolean;
  open_weights?: boolean;
}

export interface CreateAgentRequest {
  name: string;
  description: string;
  model_group?: string;
  tools?: string[];
  skills?: string[];
  /** Sent on create; materialized into Cedar policies server-side. */
  sandbox_policy?: SandboxPolicy;
  sandbox_limits?: SandboxLimits;
}

export interface UpdateAgentRequest {
  name?: string;
  description?: string;
  model_group?: string;
  enabled?: boolean;
  tools?: string[];
  skills?: string[];
  /** When set, re-materializes Cedar policies for this agent. */
  sandbox_policy?: SandboxPolicy;
  sandbox_limits?: SandboxLimits;
}

export interface SpaceResponse {
  id: string;
  name: string;
  chat_count: number;
  created_at: string;
  updated_at: string;
}

export interface SpaceWithChats extends SpaceResponse {
  chats: ChatResponse[];
}

export interface CreateSpaceRequest {
  name: string;
}

export interface ChatResponse {
  id: string;
  space_id: string | null;
  agent_id: string;
  title: string | null;
  archived_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface CreateChatRequest {
  space_id?: string;
  agent_id: string;
  title?: string;
}

export interface Attachment {
  filename: string;
  content_type: string;
  size_bytes: number;
  owner: string;
  path: string;
  url?: string;
}

export interface FileEntry {
  id: string;
  size: number;
  date: Date;
  type: "folder" | "file";
  parent: string;
}

export interface Contact {
  id: string;
  user_id: string;
  name: string;
  phone?: string;
  email?: string;
  company?: string;
  job_title?: string;
  notes?: string;
  avatar?: string;
  created_at: string;
  updated_at: string;
}

export function indexContactsById(contacts: Contact[]): Record<string, Contact> {
  return Object.fromEntries(contacts.map((c) => [c.id, c]));
}

export type MessageToolStatus = "pending" | "resolved" | "denied";

export type MessageTool =
  | { type: "HumanInTheLoop"; data: { reason: string; debugger_url: string; status: MessageToolStatus; response: string | null } }
  | { type: "Question"; data: { question: string; options: string[]; status: MessageToolStatus; response: string | null } }
  | { type: "TaskCompletion"; data: { task_id: string; chat_id: string | null; status: string } }
  | { type: "VaultApproval"; data: { query: string; reason: string; env_var_prefix: string | null; status: MessageToolStatus; response: string | null } }
  | { type: "ServiceApproval"; data: { action: string; manifest: Record<string, unknown>; previous_manifest: Record<string, unknown> | null; status: MessageToolStatus; response: string | null } };

// Typed HITL model — mirrors Rust `inference::hitl::*`. UI components read
// from `tc.hitl` for pending HITL prompts and submit typed `HitlResponse`
// payloads via `/api/chats/{id}/tool-calls/resolve`. The legacy `tool_data`
// + `MessageTool` types above remain as a transitional type declaration but
// the FE no longer reads them; backend will stop emitting them once all
// historical rows are migrated.

/// Why the inference loop paused — discriminated by `type`. New pause
/// causes (background tasks, webhooks) add variants here and on the Rust
/// side. UI handlers switch on `type`.
export type PauseReason =
  | { type: "Hitl" };

export type HitlRequest =
  | { type: "Question"; data: { options: string[] } }
  | { type: "Takeover"; data: { reason: string; debugger_url: string } }
  | { type: "App"; data: { action: string; manifest: Record<string, unknown>; previous_manifest: Record<string, unknown> | null } }
  | { type: "Credential"; data: { query: string; reason: string } };

export type VaultField =
  | "Password"
  | "Username"
  | { Custom: { name: string } };

export type CredentialTarget =
  | { Prefix: { env_var_prefix: string } }
  | { Single: { env_var: string; field: VaultField } };

export type GrantDuration =
  | "once"
  | "permanent"
  | { hours: number }
  | { days: number };

export type VaultGrant =
  | { type: "Granted"; data: { connection_id: string; vault_item_id: string; grant_duration: GrantDuration; target: CredentialTarget } }
  | { type: "Denied" };

export type HitlResponse =
  | { type: "Approval"; data: boolean }
  | { type: "Choice"; data: string }
  | { type: "Vault"; data: VaultGrant };

export interface HitlDelivery {
  channel_id: string;
  external_message_id: string;
  delivered_at: string;
}

export interface Hitl {
  prompt: string;
  url: string;
  request: HitlRequest;
  status: MessageToolStatus;
  response: HitlResponse | null;
  delivery: HitlDelivery | null;
}

export type TaskEvent =
  | { type: "Completion"; data: { task_id: string; chat_id: string | null; status: string; summary: string | null; deliverables: unknown[] } }
  | { type: "Deferred"; data: { task_id: string; delay_minutes: number; reason: string } };

export function isPendingHitl(te: { hitl?: Hitl | null }): boolean {
  return te.hitl?.status === "pending";
}

export function hitlResponseText(hitl: Hitl): string | null {
  if (!hitl.response) return null;
  switch (hitl.response.type) {
    case "Approval":
      return hitl.response.data ? "Approved" : "Denied";
    case "Choice":
      return hitl.response.data;
    case "Vault":
      return hitl.response.data.type === "Granted" ? "Granted" : "Denied";
  }
}

export type MessageEvent =
  | { type: "TaskCompletion"; data: { task_id: string; chat_id: string | null; status: string; summary?: string; schema?: Record<string, unknown> } }
  | { type: "TaskDeferred"; data: { task_id: string; delay_minutes: number; reason: string } };

export type MessageStatus = "executing" | "paused" | "completed" | "failed" | "cancelled";

export interface ToolCall {
  id: string;
  chat_id: string;
  message_id: string;
  turn: number;
  provider_call_id: string;
  name: string;
  arguments: Record<string, unknown>;
  result: string;
  success: boolean;
  duration_ms: number;
  /** New typed HITL pause marker. UI prefers this when present. */
  hitl?: Hitl | null;
  /** New typed task-control terminal signal. */
  task_event?: TaskEvent | null;
  tool_data?: MessageTool;
  system_prompt?: string;
  description?: string;
  turn_text?: string;
  /** Reasoning emitted by the model right before this tool call (first of turn). */
  turn_reasoning?: { id?: string | null; content: string; signature?: string | null } | null;
  created_at: string;
}

/** Parsed slash invocation captured on the user-role Message. The raw
 *  `content` field is preserved verbatim ("/weather London" or "@developer hi");
 *  this carries the structured form for chip rendering and dispatch. */
export type MessageCommand =
  | { type: "skill"; name: string; prompt: string }
  | { type: "command"; name: string; args: string };

export interface MessageResponse {
  id: string;
  chat_id: string;
  role: "user" | "agent" | "taskcompletion" | "contact" | "livecall" | "system";
  content: string;
  agent_id?: string;
  event?: MessageEvent;
  attachments?: Attachment[];
  contact_id?: string;
  status?: MessageStatus;
  reasoning?: string;
  tool_calls?: ToolCall[];
  /** Set when the user typed `/skill ...`, `/command ...`, or `@agent ...`. */
  command?: MessageCommand;
  created_at: string;
  /** Set by mergeConsecutiveMessages — this message continues the previous agent message. */
  _continuation?: boolean;
  /** Set by computeTimeMarkers — render a day-boundary chip above this message. */
  _daySeparator?: string;
  /** Set by computeTimeMarkers — render a "N minutes later" chip above this message. */
  _gap?: string;
}

export type AppStatus = "starting" | "running" | "stopped" | "failed" | "serving" | "hibernated";

export interface AppResponse {
  id: string;
  agent_id: string;
  /** Served at `/apps/{handle}/`. */
  handle: string;
  name: string;
  description?: string;
  kind: string;
  command?: string;
  static_dir?: string;
  port?: number;
  status: AppStatus;
  manifest: Record<string, unknown>;
  url?: string;
  created_at: string;
  updated_at: string;
}

export type TaskKind =
  | { type: "Direct"; source_chat_id?: string | null }
  | { type: "Delegation"; source_agent_id: string; source_chat_id: string; resume_parent?: boolean }
  | {
      type: "Cron";
      cron_expression: string;
      timezone?: string | null;
      next_run_at?: string | null;
      source_agent_id?: string | null;
      source_chat_id?: string | null;
      mode?: "singleton" | "per_instance";
      concurrency?: "allow" | "forbid" | "replace";
      process_result?: boolean;
    }
  | {
      type: "CronRun";
      source_cron_id: string;
      source_chat_id?: string | null;
      source_agent_id?: string | null;
      fire_at: string;
      sequence_num: number;
    }
  | {
      type: "Signal";
      source_chat_id: string;
      resume_parent?: boolean;
      mode?: "Once" | "Continuous";
      expected_categories?: string[];
      expected_channels?: string[];
      expected_contacts?: string[];
      expires_at?: string | null;
      max_evaluations?: number;
      evaluation_count?: number;
    };

export interface TaskResponse {
  id: string;
  agent_id: string;
  space_id: string | null;
  chat_id: string | null;
  title: string;
  description: string;
  status: string;
  kind: TaskKind;
  run_at: string | null;
  result_summary: string | null;
  error_message: string | null;
  created_at: string;
  updated_at: string;
}

export interface CreateTaskRequest {
  agent_id: string;
  space_id?: string;
  chat_id?: string;
  title: string;
  description?: string;
  source_agent_id?: string;
  source_chat_id?: string;
  run_at?: string;
}

export interface TaskUpdateEvent {
  task_id: string;
  status: string;
  title: string;
  chat_id: string | null;
  source_chat_id: string | null;
  result_summary: string | null;
}

export type ExecutionKind =
  | "inference"
  | "task"
  | "memory"
  | "app"
  | "scheduled"
  | "system";

export type ExecutionStatus = "queued" | "running" | "waiting" | "cancelling";

export interface ExecutionSource {
  type: "chat" | "task" | "schedule" | "system";
  id?: string;
}

export interface Execution {
  id: string;
  title: string;
  agentName?: string;
  kind: ExecutionKind;
  status: ExecutionStatus;
  action?: string;
  source?: ExecutionSource;
  relatedChatIds?: string[];
  startedAt: string;
  canCancel: boolean;
}

export interface ActivitySnapshot {
  executions: Execution[];
}

const DEFAULT_AGENT_NAMES: Record<string, string> = {
  system: "Assistant",
  researcher: "Researcher",
  developer: "Developer",
};

function titleCase(s: string): string {
  return s.replace(/\b\w/g, (c) => c.toUpperCase());
}

export function agentDisplayName(
  agentId: string | undefined,
  agentName?: string,
): string {
  if (agentName && agentName !== agentId) return titleCase(agentName);
  if (!agentId) return "Assistant";
  const name = DEFAULT_AGENT_NAMES[agentId] ?? agentId.replace(/-/g, " ");
  return titleCase(name);
}

export type CredentialData =
  | { type: "BrowserProfile" }
  | { type: "UsernamePassword"; data: { username: string } }
  | { type: "ApiKey" };

export interface CredentialResponse {
  id: string;
  name: string;
  provider: string;
  data: CredentialData;
  created_at: string;
  updated_at: string;
}

export type NotificationData =
  | { type: "App"; app_handle: string; action: string }
  | { type: "Agent"; agent_id: string; chat_id: string }
  | { type: "Task"; task_id: string }
  | { type: "System" }
  | { type: "Security" };

export type NotificationLevel = "info" | "success" | "warning" | "error";

export interface Notification {
  id: string;
  data: NotificationData;
  level: NotificationLevel;
  title: string;
  body: string;
  read: boolean;
  created_at: string;
}

export interface NavigationResponse {
  spaces: SpaceWithChats[];
  standalone_chats: ChatResponse[];
}
