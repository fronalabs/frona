import { api } from "./api-client";
import type { ConfigUpdateResponse, SensitiveField } from "./config-types";

export type CredentialMethod = "api_key" | "oauth" | "aws" | "azure_entra" | "anonymous";
export type ProviderProtocol = "completions" | "responses" | "anthropic-messages" | "google-generate-content" | "cohere-chat" | "ollama" | "huggingface" | "amazon-bedrock-converse";

export interface ProviderConnection {
  credential_id?: string | null;
  provider?: string;
  adapter?: string;
  enabled?: boolean;
  base_url?: string | null;
  api_key?: SensitiveField | null;
  aws_region?: string | null;
  aws_profile?: string | null;
  azure_credential?: string | null;
  azure_api_version?: string | null;
}

export interface SavedCredential {
  credential_id: string;
  integration: string;
  name: string;
}

export interface CredentialStatus {
  credential_id?: string | null;
  method: CredentialMethod;
  state: "active" | "pending" | "removed";
  generation: number;
  version: string;
  validation_id?: string;
}

export interface ProviderInspection {
  setup: ProviderCatalogEntry | null;
  handle: string;
  provider: string;
  adapter: string;
  configuration: ProviderConnection;
  pending_removal: boolean;
  effective_authentication: { method: CredentialMethod | null; source: string } | null;
  authentication_methods: {
    id: string;
    method: CredentialMethod;
    priority: number;
    protocols: ProviderProtocol[];
    login_available: boolean;
    validation_available: boolean;
  }[];
  credentials: CredentialStatus[];
  affected_groups: string[];
}

export interface ProviderFieldInfo {
  id: string;
  target: { kind: "configuration"; path: string } | { kind: "credential"; field: string };
  label: string;
  required: boolean;
  sensitive: boolean;
  schema: Record<string, unknown>;
  suggested_env: string[];
}

export interface ProviderCatalogEntry {
  id: string;
  name: string;
  description: string | null;
  documentation_url: string | null;
  logo_url: string | null;
  adapter: string;
  default_base_url: string | null;
  api_surfaces: ProviderProtocol[];
  auth_methods: {
    id: string;
    credential_method: CredentialMethod;
    access_mode: string;
    priority: number;
    protocols: ProviderProtocol[];
    interaction: "form" | "backend_login" | "ambient_credentials" | "none";
    persistence: "database" | "none";
    fields: ProviderFieldInfo[];
    validation_available: boolean;
  }[];
  fields: ProviderFieldInfo[];
  configuration_defaults: ProviderConnection;
  sources: string[];
  warnings: string[];
  catalog_available: boolean;
  effective_protocols: ProviderProtocol[] | null;
}

export interface ProviderCatalog {
  providers: ProviderCatalogEntry[];
  source_status: Record<string, {
    source: string;
    origin: "bundled" | "cache" | "remote" | null;
    state: "fresh" | "stale" | "unavailable";
    version: string | null;
    fetched_at: string | null;
    last_error: string | null;
  }>;
}

export function getProviderCatalog(): Promise<ProviderCatalog> {
  return api.get<ProviderCatalog>("/api/config/provider-catalog");
}

export type CredentialInput =
  | { source: "api_key"; api_key: string }
  | { source: "database"; method: CredentialMethod }
  | { source: "environment"; variable: string }
  | { source: "ambient"; method: CredentialMethod }
  | { source: "anonymous" };

export type SettingApplicability =
  | { op: "all"; expressions: SettingApplicability[] }
  | { op: "any"; expressions: SettingApplicability[] }
  | { op: "not"; expression: SettingApplicability }
  | { op: "in"; config_path: string; values: unknown[] };

export interface ModelSettingInfo {
  id: string;
  storage: { kind: "typed" | "extra_params"; config_path: string } | null;
  request_path: string | null;
  catalog_path: string | null;
  label: string;
  description: string | null;
  group: string;
  scope: "provider_request" | "frona_runtime";
  schema: Record<string, unknown>;
  applicability: SettingApplicability | null;
  support: "typed" | "extra_params" | "unsupported_by_adapter" | "configured_unverified";
  sources: string[];
}

export interface ProviderModelRow {
  id: string;
  name: string | null;
  description: string | null;
  context_window: number | null;
  max_tokens: number | null;
  availability: "account" | "catalog" | "unverified" | "configured" | "recipe";
  configured_in: string[];
  sources: string[];
  warnings: string[];
  suggested_protocol: ProviderProtocol | null;
  protocols: { api: ProviderProtocol; available: boolean; settings: ModelSettingInfo[]; warnings: string[] }[];
  capabilities: { reasoning: boolean | null; tool_call: boolean | null; structured_output: boolean | null; input: string[]; output: string[] };
}
export interface ModelDirectory {
  connection: string;
  credential_method: CredentialMethod | null;
  access_mode: "api" | "subscription";
  source: string;
  directory_status: "live" | "catalog_fallback" | "configured_only" | "live_error" | "unavailable";
  source_status: Record<string, Record<string, unknown>>;
  manual_entry: boolean;
  models: ProviderModelRow[];
}
export interface ProviderValidation {
  validation_id: string;
  credential: CredentialStatus;
  models: ProviderModelRow[] | null;
}
export interface DraftProvider {
  manual_models?: string[];
  config: ProviderConnection;
  validation_id: string;
  method: CredentialMethod;
  source: string;
}
export type ProviderModelListing = ModelDirectory;
export interface CredentialMutation {
  credential: CredentialStatus;
  affected_groups: string[];
  unavailable_models: Record<string, [string, string][]>;
}

export interface LoginAttempt {
  id: string;
  status: "pending" | "validated" | "failed" | "expired" | "cancelled";
  challenge: { kind: "redirect" | "device_code"; url: string; user_code?: string; message?: string } | null;
  credential_id: string | null;
  error?: string;
}

const path = (handle: string) => `/api/config/providers/${encodeURIComponent(handle.trim().toLowerCase())}`;

export const providerAdmin = {
  list: () => api.get<{ providers: ProviderInspection[] }>("/api/config/providers"),
  savedCredentials: () => api.get<SavedCredential[]>("/api/config/provider-credentials"),
  environmentVariables: () => api.get<string[]>("/api/config/environment-variables"),
  accept: (handle: string, draft: DraftProvider) => api.post<CredentialStatus>(`${path(handle)}/credentials`, {config: draft.config, validation_id: draft.validation_id, method: draft.method, source: draft.source}),
  inspect: (handle: string) => api.get<ProviderInspection>(path(handle)),
  inspectDraft: (handle: string, config: ProviderConnection) => api.post<ProviderInspection>(`${path(handle)}/inspect`, { config }),
  validate: (handle: string, config: ProviderConnection, credential: CredentialInput) => api.post<ProviderValidation>(`${path(handle)}/validate`, { config, credential }),
  models: (handle: string, manualModels: string[] = []) => {
    const query = new URLSearchParams();
    for (const model of manualModels) query.append("manual_model", model);
    return api.get<ProviderModelListing>(`${path(handle)}/models${query.size ? `?${query}` : ""}`);
  },
  draftModels: (handle: string, draft: DraftProvider) => api.post<ProviderModelListing>(`${path(handle)}/models`, {
    config: draft.config, validation_id: draft.validation_id, method: draft.method, source: draft.source,
    ...(draft.manual_models ? { manual_models: draft.manual_models } : {}),
  }),
  credentialModels: (handle: string, config: ProviderConnection, manualModels: string[] = []) => api.post<ProviderModelListing>(`${path(handle)}/models`, {
    // Config responses mask API keys as objects; those markers aren't credentials.
    config: config.api_key && typeof config.api_key === "object" ? { ...config, api_key: null } : config,
    manual_models: manualModels,
  }),
  edit: (handle: string, config: ProviderConnection, revision: string) => api.put<ConfigUpdateResponse>(path(handle), {
    config, expected_persisted_revision: revision,
  }),
  delete: (handle: string, revision: string) => api.delete<ConfigUpdateResponse>(path(handle), { expected_persisted_revision: revision }),
  logout: (handle: string, method: CredentialMethod, generation: number) => api.delete<CredentialMutation>(`${path(handle)}/credentials/${method}`, { expected_generation: generation }),
  discard: (handle: string, validationId: string) => api.delete<{ discarded: boolean }>(`${path(handle)}/drafts/${encodeURIComponent(validationId)}`),
  startLogin: (handle: string, config: ProviderConnection, method: CredentialMethod) => api.post<LoginAttempt>(`${path(handle)}/login/start`, { config, method }),
  loginStatus: (handle: string, attempt: string) => api.get<LoginAttempt>(`${path(handle)}/login/${encodeURIComponent(attempt)}`),
  completeLogin: (handle: string, attempt: string, code: string) => api.post<LoginAttempt>(`${path(handle)}/login/${encodeURIComponent(attempt)}/complete`, { code }),
  cancelLogin: (handle: string, attempt: string) => api.delete<LoginAttempt>(`${path(handle)}/login/${encodeURIComponent(attempt)}`),
};
