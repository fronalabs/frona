export interface UserInfo {
  id: string;
  username: string;
  email: string;
  name: string;
  needs_setup?: boolean;
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
  username: string;
  email: string;
  name: string;
  password: string;
}

export interface SsoStatus {
  enabled: boolean;
  sso_only: boolean;
}

export interface Agent {
  id: string;
  name: string;
  description: string;
  model_group: string;
  enabled: boolean;
  tools: string[];
  avatar: string | null;
  identity: Record<string, string>;
  chat_count: number;
  created_at: string;
  updated_at: string;
}

export interface CreateAgentRequest {
  name: string;
  description: string;
  model_group?: string;
  tools?: string[];
}

export interface UpdateAgentRequest {
  name?: string;
  description?: string;
  model_group?: string;
  enabled?: boolean;
  tools?: string[];
}

// Space types
export interface SpaceResponse {
  id: string;
  name: string;
  created_at: string;
  updated_at: string;
}

export interface SpaceWithChats extends SpaceResponse {
  chats: ChatResponse[];
}

export interface CreateSpaceRequest {
  name: string;
}

// Chat types
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

// File attachment types
export interface Attachment {
  filename: string;
  content_type: string;
  size_bytes: number;
  owner: string;
  path: string;
  url?: string;
}

// Contact types
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

// Message types
export type MessageToolStatus = "pending" | "resolved" | "denied";

export type MessageTool =
  | { type: "HumanInTheLoop"; data: { reason: string; debugger_url: string; status: MessageToolStatus; response: string | null } }
  | { type: "Question"; data: { question: string; options: string[]; status: MessageToolStatus; response: string | null } }
  | { type: "TaskCompletion"; data: { task_id: string; chat_id: string | null; status: string } }
  | { type: "VaultApproval"; data: { query: string; reason: string; env_var_prefix: string | null; status: MessageToolStatus; response: string | null } }
  | { type: "ServiceApproval"; data: { action: string; manifest: Record<string, unknown>; previous_manifest: Record<string, unknown> | null; status: MessageToolStatus; response: string | null } };

export interface MessageResponse {
  id: string;
  chat_id: string;
  role: "user" | "agent" | "toolresult" | "taskcompletion" | "contact" | "livecall";
  content: string;
  agent_id?: string;
  tool_calls?: unknown[];
  tool_call_id?: string;
  tool?: MessageTool;
  attachments?: Attachment[];
  contact_id?: string;
  created_at: string;
}

// App types
export type AppStatus = "starting" | "running" | "stopped" | "failed" | "serving" | "hibernated";

export interface AppResponse {
  id: string;
  agent_id: string;
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

// Task types
export type TaskKind =
  | { type: "Direct" }
  | { type: "Delegation"; source_agent_id: string; source_chat_id: string };

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

// Agent display name defaults
const DEFAULT_AGENT_NAMES: Record<string, string> = {
  system: "Assistant",
  researcher: "Researcher",
  developer: "Developer",
};

export function agentDisplayName(
  agentId: string | undefined,
  agentName?: string,
): string {
  if (agentName && agentName !== agentId) return agentName;
  if (!agentId) return "Assistant";
  return DEFAULT_AGENT_NAMES[agentId] ?? agentId;
}

// Tool call types
export interface ToolCallStatus {
  name: string;
  description: string | null;
  status: "running" | "done";
}

// Credential types
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

// Navigation types
export interface NavigationResponse {
  spaces: SpaceWithChats[];
  standalone_chats: ChatResponse[];
}
