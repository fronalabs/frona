export interface UserInfo {
  id: string;
  email: string;
  name: string;
}

export interface AuthResponse {
  token?: string;
  user: UserInfo;
}

export interface LoginRequest {
  email: string;
  password: string;
}

export interface RegisterRequest {
  email: string;
  name: string;
  password: string;
}

export interface Agent {
  id: string;
  name: string;
  description: string;
  system_prompt: string;
  model_group: string;
  enabled: boolean;
  tools: string[];
  created_at: string;
  updated_at: string;
}

export interface CreateAgentRequest {
  name: string;
  description: string;
  system_prompt: string;
  model_group?: string;
  tools?: string[];
}

export interface UpdateAgentRequest {
  name?: string;
  description?: string;
  system_prompt?: string;
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
  created_at: string;
  updated_at: string;
}

export interface CreateChatRequest {
  space_id?: string;
  agent_id: string;
  title?: string;
}

// Message types
export type MessageToolStatus = "pending" | "resolved";

export type MessageTool =
  | { type: "HumanInTheLoop"; data: { reason: string; debugger_url: string; status: MessageToolStatus; response: string | null } }
  | { type: "Question"; data: { question: string; options: string[]; status: MessageToolStatus; response: string | null } }
  | { type: "Warning"; data: { message: string } }
  | { type: "Info"; data: { message: string } };

export interface MessageResponse {
  id: string;
  chat_id: string;
  role: "user" | "assistant" | "toolresult";
  content: string;
  tool_calls?: unknown[];
  tool_call_id?: string;
  tool?: MessageTool;
  created_at: string;
}

// Task types
export interface TaskResponse {
  id: string;
  chat_id: string | null;
  title: string;
  description: string;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface CreateTaskRequest {
  chat_id?: string;
  title: string;
  description?: string;
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
  | { type: "UsernamePassword"; data: { username: string } };

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
