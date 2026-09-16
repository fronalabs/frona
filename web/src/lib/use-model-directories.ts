"use client";
import { useCallback, useEffect, useRef, useState } from "react";
import type { ModelGroupConfig, ModelProviderConfig } from "./config-types";
import { providerAdmin, type ModelDirectory } from "./provider-admin";
import { matchingDraft, providerFingerprint, type ProviderDrafts } from "./provider-drafts";

interface CachedDirectory { key: string; data?: ModelDirectory; pending?: Promise<ModelDirectory>; expires?: number }

export function useModelDirectories(models: Record<string, ModelGroupConfig>, configs: Record<string, ModelProviderConfig>,
  drafts: ProviderDrafts, savedConfigs?: Record<string, ModelProviderConfig>) {
  const cache = useRef(new Map<string, CachedDirectory>());
  const [directories, setDirectories] = useState<Record<string, ModelDirectory>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState<Record<string, boolean>>({});
  const load = useCallback(async (handle: string, manual: string[] = [], force = false): Promise<ModelDirectory | undefined> => {
    const config = configs[handle];
    if (!config) throw new Error("Select a configured connection");
    const proof = matchingDraft(config, drafts[handle]);
    if (!proof && !config.credential_id && savedConfigs && (!savedConfigs[handle] || providerFingerprint(config) !== providerFingerprint(savedConfigs[handle]))) {
      cache.current.delete(handle);
      setLoading(previous => ({ ...previous, [handle]: false }));
      setDirectories(previous => { const next = { ...previous }; delete next[handle]; return next; });
      setErrors(previous => { const next = { ...previous }; delete next[handle]; return next; });
      return;
    }
    const key = `${providerFingerprint(config)}:${proof?.validation_id ?? "active"}`;
    let cached = cache.current.get(handle);
    if (cached && cached.key !== key) setDirectories(previous => { const next = { ...previous }; delete next[handle]; return next; });
    if (!force && cached?.key === key) {
      if (cached.pending) { try { await cached.pending; } catch { /* allow a fresh attempt */ } cached = cache.current.get(handle); }
      if (cached?.data && (cached.expires ?? 0) > Date.now() && manual.every(id => cached!.data!.models.some(model => model.id === id))) return cached.data;
    }
    setLoading(previous => ({ ...previous, [handle]: true }));
    const pending = proof ? providerAdmin.draftModels(handle, { ...proof, manual_models: manual })
      : config.credential_id ? providerAdmin.credentialModels(handle, config, manual) : providerAdmin.models(handle, manual);
    const entry: CachedDirectory = { key, pending };
    cache.current.set(handle, entry);
    try {
      const result = await pending;
      if (cache.current.get(handle) === entry) {
        entry.data = result; entry.pending = undefined; entry.expires = Date.now() + 60_000;
        setDirectories(previous => ({ ...previous, [handle]: result }));
        setErrors(previous => { const next = { ...previous }; delete next[handle]; return next; });
      }
      return result;
    } catch (error) {
      if (cache.current.get(handle) === entry) {
        cache.current.delete(handle);
        setErrors(previous => ({ ...previous, [handle]: error instanceof Error ? error.message : "Model discovery failed" }));
      }
      throw error;
    } finally { if (cache.current.get(handle) === entry || !cache.current.has(handle)) setLoading(previous => ({ ...previous, [handle]: false })); }
  }, [configs, drafts, savedConfigs]);

  const references = new Map<string, Set<string>>();
  function visit(group: ModelGroupConfig) {
    if (group.provider) {
      const ids = references.get(group.provider) ?? new Set<string>();
      if (group.model) ids.add(group.model);
      references.set(group.provider, ids);
    }
    for (const fallback of group.fallbacks ?? []) visit(fallback);
  }
  for (const group of Object.values(models)) visit(group);
  const signature = JSON.stringify([...references].sort(([a], [b]) => a.localeCompare(b)).map(([handle, ids]) => [handle, [...ids].sort()]));
  useEffect(() => {
    const references: [string, string[]][] = JSON.parse(signature);
    for (const [handle, ids] of references) void load(handle, ids).catch(() => {});
    const refresh = setInterval(() => { for (const [handle, ids] of references) void load(handle, ids).catch(() => {}); }, 60_000);
    return () => clearInterval(refresh);
  }, [signature, load]);
  return { directories, errors, loading, load };
}
