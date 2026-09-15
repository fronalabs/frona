import { providerAdmin, type DraftProvider, type ProviderConnection, type ProviderValidation } from "./provider-admin";
import type { ModelProviderConfig } from "./config-types";

export interface PreparedProviderDraft extends DraftProvider {
  fingerprint: string;
  expiresAt: number;
}
export type ProviderDrafts = Record<string, PreparedProviderDraft>;

export function normalizeProviderHandle(value: string): string {
  const handle = value.trim().toLowerCase();
  if (!/^[a-z][a-z0-9_-]{1,31}$/.test(handle)) {
    throw new Error("Use 2-32 lowercase letters, digits, hyphens or underscores, starting with a letter.");
  }
  return handle;
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") return Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([key, value]) => [key, canonical(value)]));
  return value;
}

export function providerFingerprint(config: ProviderConnection): string {
  const binding = { ...config };
  delete binding.enabled;
  return JSON.stringify(canonical(binding));
}

export function prepareProviderDraft(config: ProviderConnection, source: string, result: ProviderValidation): PreparedProviderDraft {
  return { config, source, validation_id: result.validation_id, method: result.credential.method,
    fingerprint: providerFingerprint(config), expiresAt: Date.now() + 29 * 60_000 };
}

export function matchingDraft(config: ProviderConnection, draft?: PreparedProviderDraft): PreparedProviderDraft | undefined {
  return draft && draft.expiresAt > Date.now() && draft.fingerprint === providerFingerprint(config) ? draft : undefined;
}

export async function acceptProviderDrafts(
  patch: Record<string, unknown>,
  drafts: ProviderDrafts,
  onAccepted: (handle: string, config: ModelProviderConfig) => void,
): Promise<Record<string, unknown>> {
  if (!patch.providers || typeof patch.providers !== "object") return patch;
  const providers = { ...patch.providers } as Record<string, ModelProviderConfig | null>;
  for (const [handle, config] of Object.entries(providers)) {
    const draft = drafts[handle];
    if (!config || draft?.source !== "database") continue;
    if (!matchingDraft(config, draft)) throw new Error(`${handle}: validate the connection again before saving`);
    const credential = await providerAdmin.accept(handle, draft);
    if (!credential.credential_id) throw new Error("The server did not return a credential ID");
    const accepted = { ...config, api_key: null, credential_id: credential.credential_id };
    providers[handle] = accepted;
    // Keep accepted references if a later credential or configuration save fails.
    onAccepted(handle, accepted);
  }
  return { ...patch, providers };
}

export function readPointer(value: unknown, path: string): unknown {
  if (!path.startsWith("/")) return undefined;
  for (const token of path.slice(1).split("/")) {
    const key = token.replace(/~1/g, "/").replace(/~0/g, "~");
    if (!value || typeof value !== "object" || !Object.hasOwn(value, key)) return undefined;
    value = (value as Record<string, unknown>)[key];
  }
  return value;
}

export function writePointer<T extends object>(value: T, path: string, next: unknown): T {
  if (!path.startsWith("/")) throw new Error("Expected a configuration JSON Pointer");
  const parts = path.slice(1).split("/").map(part => part.replace(/~1/g, "/").replace(/~0/g, "~"));
  function set(object: Record<string, unknown>, index: number): Record<string, unknown> {
    const key = parts[index];
    if (index === parts.length - 1) return { ...object, [key]: next };
    const child = Object.hasOwn(object, key) && object[key] && typeof object[key] === "object" ? object[key] as Record<string, unknown> : {};
    return { ...object, [key]: set(child, index + 1) };
  }
  return set(value as Record<string, unknown>, 0) as T;
}
