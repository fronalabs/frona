"use client";

import { useState, useEffect, useCallback, useRef } from "react";
import type { VaultConfig, SensitiveField } from "@/lib/config-types";
import { isSensitiveSet } from "@/lib/config-types";
import { Field, TextInput, SensitiveInput, SectionHeader, HelpTip } from "@/components/settings/field";
import { EllipsisVerticalIcon, LockClosedIcon, PlusIcon } from "@heroicons/react/24/outline";
import { api } from "@/lib/api-client";
import type { CredentialResponse } from "@/lib/types";

import { useSettings } from "@/components/settings/settings-context";
import type { TestStatus } from "@/components/settings/sections/providers-section";
import { TestStatusIcon } from "@/components/settings/sections/providers-section";

interface VaultSectionProps {
  vault: VaultConfig;
  onChange: (vault: VaultConfig) => void;
}

import { Si1password, SiBitwarden, SiVault, SiKeepassxc, SiKeeper } from "@icons-pack/react-simple-icons";

const VAULT_LOGOS: Record<string, React.ComponentType<{ className?: string; size?: number; color?: string }>> = {
  onepassword: Si1password,
  bitwarden: SiBitwarden,
  hashicorp: SiVault,
  keepass: SiKeepassxc,
  keeper: SiKeeper,
};

interface ProviderCardProps {
  id: string;
  name: string;
  enabled: boolean;
  expanded: boolean;
  testStatus?: TestStatus;
  onToggle: () => void;
  onEnabledChange: (enabled: boolean) => void;
  children: React.ReactNode;
}

function ProviderCard({ id, name, enabled, expanded, testStatus, onToggle, onEnabledChange, children }: ProviderCardProps) {
  const Logo = VAULT_LOGOS[id];

  if (!enabled) {
    return (
      <div className="flex items-center justify-between rounded-lg border border-border bg-surface-secondary px-4 py-3">
        <div className="flex items-center gap-2.5">
          {Logo && <Logo size={18} className="text-text-tertiary" />}
          <span className="text-sm text-text-secondary">{name}</span>
        </div>
        <button
          type="button"
          onClick={() => onEnabledChange(true)}
          className="rounded-lg bg-surface-tertiary px-3 py-1 text-xs font-medium text-text-secondary hover:bg-accent hover:text-surface transition"
        >
          Enable
        </button>
      </div>
    );
  }

  return (
    <div className="rounded-lg border border-border bg-surface-secondary">
      <div className="flex w-full items-center justify-between px-4 py-3">
        <button
          type="button"
          onClick={onToggle}
          className="flex flex-1 items-center gap-2.5 text-left hover:opacity-80 transition"
        >
          {Logo && <Logo size={18} className="text-text-tertiary" />}
          <span className="text-sm font-medium text-text-primary">{name}</span>
          {testStatus && testStatus !== "idle" && <TestStatusIcon status={testStatus} />}
        </button>
        <button
          type="button"
          onClick={(e) => { e.stopPropagation(); onEnabledChange(false); }}
          className="relative inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors bg-accent"
        >
          <span className="pointer-events-none inline-block h-5 w-5 rounded-full bg-surface shadow transform transition-transform translate-x-5" />
        </button>
      </div>
      {expanded && (
        <div className="space-y-4 px-4 pb-4">
          {children}
        </div>
      )}
    </div>
  );
}

type CredentialType = "BrowserProfile" | "UsernamePassword" | "ApiKey";

const CREDENTIAL_TYPES: { value: CredentialType; label: string }[] = [
  { value: "ApiKey", label: "API Key" },
  { value: "UsernamePassword", label: "Password" },
  { value: "BrowserProfile", label: "Browser Profile" },
];


interface VaultItem {
  id: string;
  credType: CredentialType;
  name: string;
  username: string;
  password: string;
  apiKey: string;
  isNew: boolean;
  isDeleted: boolean;
  isEdited: boolean;
}

let nextTempId = 0;


function SecretField({ label, value, onChange, placeholder }: { label: string; value: string; onChange: (v: string) => void; placeholder?: string }) {
  const [editing, setEditing] = useState(false);

  if (!editing) {
    return (
      <Field label={label}>
        <div className="flex items-center gap-2">
          <span className="text-sm text-text-primary">Configured</span>
          <button
            type="button"
            onClick={() => setEditing(true)}
            className="text-xs text-accent hover:underline"
          >
            Change
          </button>
        </div>
      </Field>
    );
  }

  return (
    <Field label={label}>
      <input
        type="password"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none"
      />
    </Field>
  );
}

function credResponseToItem(cred: CredentialResponse): VaultItem {
  const credType: CredentialType =
    cred.data.type === "UsernamePassword" ? "UsernamePassword"
    : cred.data.type === "ApiKey" ? "ApiKey"
    : "BrowserProfile";
  return {
    id: cred.id,
    credType,
    name: cred.name,
    username: cred.data.type === "UsernamePassword" ? cred.data.data.username : "",
    password: "",
    apiKey: "",
    isNew: false,
    isDeleted: false,
    isEdited: false,
  };
}

function LocalVaultPanel({ expanded, onToggle }: { expanded: boolean; onToggle: () => void }) {
  const { setModified, register, unregister } = useSettings();
  const [items, setItems] = useState<VaultItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [contextMenuId, setContextMenuId] = useState<string | null>(null);
  const [addMenu, setAddMenu] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const contextRef = useRef<HTMLDivElement>(null);
  const addMenuRef = useRef<HTMLDivElement>(null);

  const fetchItems = useCallback(async () => {
    try {
      const data = await api.get<CredentialResponse[]>("/api/vaults/local/items?max_results=100");
      setItems(data.map(credResponseToItem));
    } catch {
      setItems([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchItems(); }, [fetchItems]);

  // Close context menu on outside click
  useEffect(() => {
    if (!contextMenuId) return;
    const handleClick = (e: MouseEvent) => {
      if (contextRef.current && !contextRef.current.contains(e.target as Node)) {
        setContextMenuId(null);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [contextMenuId]);

  // Close add menu on outside click
  useEffect(() => {
    if (!addMenu) return;
    const handleClick = (e: MouseEvent) => {
      if (addMenuRef.current && !addMenuRef.current.contains(e.target as Node)) {
        setAddMenu(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [addMenu]);

  const selected = selectedId ? items.find((i) => i.id === selectedId) ?? null : null;

  const isDirty = items.some((i) => i.isNew || i.isEdited || i.isDeleted);

  const updateItem = (id: string, updates: Partial<VaultItem>) => {
    setItems((prev) => prev.map((i) => i.id === id ? { ...i, ...updates, isEdited: !i.isNew ? true : i.isEdited } : i));
  };

  const startNew = useCallback((credType: CredentialType) => {
    const id = `new-${++nextTempId}`;
    const item: VaultItem = { id, credType, name: "", username: "", password: "", apiKey: "", isNew: true, isDeleted: false, isEdited: false };
    setItems((prev) => [...prev, item]);
    setSelectedId(id);
    setAddMenu(false);
  }, []);

  const markDeleted = useCallback(async (id: string) => {
    const item = items.find((i) => i.id === id);
    if (!item) return;
    if (item.isNew) {
      setItems((prev) => prev.filter((i) => i.id !== id));
    } else {
      try {
        await api.delete(`/api/vaults/local/items/${id}`);
        setItems((prev) => prev.filter((i) => i.id !== id));
      } catch {
        // ignore
      }
    }
    if (selectedId === id) setSelectedId(null);
    setContextMenuId(null);
  }, [items, selectedId]);

  const buildBody = (item: VaultItem, isCreate: boolean): Record<string, string> | null => {
    if (!item.name.trim()) return null;
    if (item.credType === "BrowserProfile") return { type: "BrowserProfile", name: item.name };
    if (item.credType === "UsernamePassword") {
      if (isCreate && (!item.username.trim() || !item.password.trim())) return null;
      const body: Record<string, string> = { type: "UsernamePassword", name: item.name, username: item.username };
      if (item.password.trim()) body.password = item.password;
      return body;
    }
    if (isCreate && !item.apiKey.trim()) return null;
    const body: Record<string, string> = { type: "ApiKey", name: item.name };
    if (item.apiKey.trim()) body.api_key = item.apiKey;
    return body;
  };

  const handleSave = useCallback(async () => {
    setSubmitting(true);
    try {
      for (const item of items) {
        if (item.isNew) {
          const body = buildBody(item, true);
          if (body) await api.post("/api/vaults/local/items", body);
        } else if (item.isEdited) {
          const body = buildBody(item, false);
          if (body) await api.put(`/api/vaults/local/items/${item.id}`, body);
        }
      }
      await fetchItems();
      setSelectedId(null);
    } catch {
      // ignore
    } finally {
      setSubmitting(false);
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [items, fetchItems]);

  const discard = useCallback(() => {
    setSelectedId(null);
    setLoading(true);
    fetchItems();
  }, [fetchItems]);

  useEffect(() => {
    register("credentials", { save: handleSave, discard });
  }, [register, handleSave, discard]);

  useEffect(() => {
    return () => unregister("credentials");
  }, [unregister]);

  useEffect(() => {
    setModified("credentials", isDirty);
  }, [isDirty, setModified]);

  const itemCount = items.filter((i) => !i.isDeleted).length;

  const typeLabel = (t: CredentialType) =>
    t === "BrowserProfile" ? "Browser" : t === "UsernamePassword" ? "Password" : "API Key";

  return (
    <div className="rounded-lg border border-border bg-surface-secondary">
      <div
        role="button"
        tabIndex={0}
        onClick={onToggle}
        onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") onToggle(); }}
        className="flex w-full items-center justify-between px-4 py-3 cursor-pointer hover:opacity-80 transition"
      >
        <div className="flex flex-1 items-center gap-2.5">
          <LockClosedIcon className="h-5 w-5 text-text-tertiary" />
          <span className="inline-flex items-center gap-1" onClick={(e) => e.stopPropagation()}>
            <span className="text-sm font-medium text-text-primary leading-none">Local</span>
            <HelpTip content="The local vault stores credentials on the server. Agents can use these to log into websites, authenticate with APIs, or access protected services on your behalf." />
          </span>
        </div>
        <span className={`rounded-lg px-3 py-1 text-xs font-medium ${itemCount > 0 ? "bg-accent/10 text-accent" : "bg-surface-tertiary text-text-secondary"}`}>
          {itemCount > 0 ? `${itemCount} item${itemCount !== 1 ? "s" : ""}` : "Empty"}
        </span>
      </div>
      {expanded && (
        <div className="px-4 pb-4">
          {loading ? (
            <div className="flex items-center justify-center py-4">
              <svg className="h-4 w-4 animate-spin text-text-tertiary" viewBox="0 0 24 24" fill="none">
                <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
              </svg>
            </div>
          ) : (
            <div className="rounded-lg border border-border bg-surface overflow-hidden">
              <div className="flex h-80">
                {/* Left: item list */}
                <div className="w-44 shrink-0 border-r border-border overflow-y-auto flex flex-col">
                  <div className="flex-1">
                    {items.length === 0 && (
                      <p className="text-xs text-text-tertiary p-3">No items</p>
                    )}
                    {items.map((item) => (
                      <div
                        key={item.id}
                        className={`relative group flex items-center border-b border-border transition cursor-pointer ${
                          selectedId === item.id
                            ? "bg-accent/10"
                            : "hover:bg-surface-tertiary"
                        }`}
                      >
                        <button
                          type="button"
                          onClick={() => setSelectedId(item.id)}
                          className="flex-1 text-left px-3 py-2 min-w-0"
                        >
                          <span className={`block text-sm truncate ${selectedId === item.id ? "text-accent font-medium" : "text-text-secondary"}`}>
                            {item.name || (item.isNew ? "New item" : "Unnamed")}
                          </span>
                          <span className="block text-[10px] text-text-tertiary truncate">
                            {typeLabel(item.credType)}
                            {item.credType === "UsernamePassword" && item.username ? ` · ${item.username}` : ""}
                          </span>
                        </button>
                        <div className="relative shrink-0 pr-1">
                          <button
                            type="button"
                            onClick={(e) => { e.stopPropagation(); setContextMenuId(contextMenuId === item.id ? null : item.id); }}
                            className="p-1 rounded text-text-tertiary opacity-0 group-hover:opacity-100 hover:text-text-primary transition"
                          >
                            <EllipsisVerticalIcon className="h-4 w-4" />
                          </button>
                          {contextMenuId === item.id && (
                            <div ref={contextRef} className="absolute right-0 top-full mt-0.5 w-28 bg-surface border border-border rounded-lg shadow-lg z-20">
                              <button
                                type="button"
                                onClick={() => markDeleted(item.id)}
                                className="w-full text-left px-3 py-2 text-sm text-danger hover:bg-surface-tertiary transition rounded-lg"
                              >
                                Delete
                              </button>
                            </div>
                          )}
                        </div>
                      </div>
                    ))}
                  </div>
                  <div className="relative border-t border-border" ref={addMenuRef}>
                    <button
                      type="button"
                      onClick={() => setAddMenu((v) => !v)}
                      className="w-full flex items-center justify-center gap-1 px-3 py-2 text-xs text-text-tertiary hover:text-text-primary hover:bg-surface-tertiary transition"
                    >
                      <PlusIcon className="h-3.5 w-3.5" />
                      Add
                    </button>
                    {addMenu && (
                      <div className="absolute bottom-full left-0 mb-1 w-40 bg-surface border border-border rounded-lg shadow-lg z-20">
                        {CREDENTIAL_TYPES.map(({ value, label }) => (
                          <button
                            key={value}
                            onClick={() => startNew(value)}
                            className="w-full text-left px-3 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition first:rounded-t-lg last:rounded-b-lg"
                          >
                            {label}
                          </button>
                        ))}
                      </div>
                    )}
                  </div>
                </div>

                {/* Right: detail/edit form */}
                <div className="flex-1 overflow-y-auto p-3">
                  {!selected && (
                    <p className="text-sm text-text-tertiary text-center mt-12">Select an item</p>
                  )}
                  {selected && (
                    <div className="space-y-3">
                      <TextInput
                        label="Name"
                        value={selected.name}
                        onChange={(v) => updateItem(selected.id, { name: v })}
                        placeholder="Name"
                      />
                      {selected.credType === "UsernamePassword" && (
                        <>
                          <TextInput
                            label="Username"
                            value={selected.username}
                            onChange={(v) => updateItem(selected.id, { username: v })}
                            placeholder="Username"
                          />
                          {selected.isNew ? (
                            <TextInput
                              label="Password"
                              value={selected.password}
                              onChange={(v) => updateItem(selected.id, { password: v })}
                              placeholder="Password"
                              type="password"
                            />
                          ) : (
                            <SecretField
                              label="Password"
                              value={selected.password}
                              onChange={(v) => updateItem(selected.id, { password: v })}
                              placeholder="Enter new password"
                            />
                          )}
                        </>
                      )}
                      {selected.credType === "ApiKey" && (
                        selected.isNew ? (
                          <TextInput
                            label="API Key"
                            value={selected.apiKey}
                            onChange={(v) => updateItem(selected.id, { apiKey: v })}
                            placeholder="API Key"
                            type="password"
                          />
                        ) : (
                          <SecretField
                            label="API Key"
                            value={selected.apiKey}
                            onChange={(v) => updateItem(selected.id, { apiKey: v })}
                            placeholder="Enter new key"
                          />
                        )
                      )}
                      {(selected.isNew || selected.isEdited) && (
                        <p className="text-[10px] text-text-tertiary italic">
                          {selected.isNew ? "New — will be created on save" : "Modified — will be updated on save"}
                        </p>
                      )}
                    </div>
                  )}
                </div>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** Get string value from a SensitiveField, or null if redacted/empty */
function sensitiveStr(v: SensitiveField): string | null {
  if (typeof v === "string" && v.length > 0) return v;
  return null;
}

type VaultTestConfig =
  | { type: "OnePassword"; service_account_token: string; default_vault_id: string | null }
  | { type: "Bitwarden"; client_id: string; client_secret: string; master_password: string; server_url: string | null }
  | { type: "Hashicorp"; address: string; token: string; mount_path: string | null }
  | { type: "KeePass"; file_path: string; master_password: string }
  | { type: "Keeper"; app_key: string; server: string | null };

/** Build test payloads for each vault provider that has enough config to test.
 *  Only includes providers where all required secrets are actual string values (not redacted). */
function buildTestable(vault: VaultConfig): { id: string; provider: string; config: VaultTestConfig }[] {
  const result: { id: string; provider: string; config: VaultTestConfig }[] = [];

  const opToken = sensitiveStr(vault.onepassword_service_account_token);
  if (opToken) {
    result.push({
      id: "onepassword",
      provider: "one_password",
      config: { type: "OnePassword", service_account_token: opToken, default_vault_id: vault.onepassword_vault_id ?? null },
    });
  }

  const bwId = vault.bitwarden_client_id;
  const bwSecret = sensitiveStr(vault.bitwarden_client_secret);
  const bwPassword = sensitiveStr(vault.bitwarden_master_password);
  if (bwId && bwSecret && bwPassword) {
    result.push({
      id: "bitwarden",
      provider: "bitwarden",
      config: { type: "Bitwarden", client_id: bwId, client_secret: bwSecret, master_password: bwPassword, server_url: vault.bitwarden_server_url ?? null },
    });
  }

  const hcAddr = vault.hashicorp_address;
  const hcToken = sensitiveStr(vault.hashicorp_token);
  if (hcAddr && hcToken) {
    result.push({
      id: "hashicorp",
      provider: "hashicorp",
      config: { type: "Hashicorp", address: hcAddr, token: hcToken, mount_path: vault.hashicorp_mount ?? null },
    });
  }

  const kpPath = vault.keepass_path;
  const kpPass = sensitiveStr(vault.keepass_password);
  if (kpPath && kpPass) {
    result.push({
      id: "keepass",
      provider: "kee_pass",
      config: { type: "KeePass", file_path: kpPath, master_password: kpPass },
    });
  }

  const krKey = sensitiveStr(vault.keeper_app_key);
  if (krKey) {
    result.push({
      id: "keeper",
      provider: "keeper",
      config: { type: "Keeper", app_key: krKey, server: null },
    });
  }

  return result;
}

const VAULT_CLEAR_FIELDS: Record<string, Partial<VaultConfig>> = {
  onepassword: { onepassword_service_account_token: { is_set: false }, onepassword_vault_id: null },
  bitwarden: { bitwarden_client_id: null, bitwarden_client_secret: { is_set: false }, bitwarden_master_password: { is_set: false }, bitwarden_server_url: null },
  hashicorp: { hashicorp_address: null, hashicorp_token: { is_set: false }, hashicorp_mount: null },
  keepass: { keepass_path: null, keepass_password: { is_set: false } },
  keeper: { keeper_app_key: { is_set: false } },
};

export function VaultSection({ vault, onChange }: VaultSectionProps) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [disabledProviders, setDisabledProviders] = useState<Record<string, boolean>>({});
  const [testStatuses, setTestStatuses] = useState<Record<string, TestStatus>>({});
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const testingRef = useRef(false);

  const toggle = (key: string) => {
    setExpanded((prev) => ({ ...prev, [key]: !prev[key] }));
  };

  const setProviderEnabled = (id: string, enabled: boolean) => {
    if (!enabled) {
      const clearFields = VAULT_CLEAR_FIELDS[id];
      if (clearFields) onChange({ ...vault, ...clearFields } as VaultConfig);
      setDisabledProviders((prev) => ({ ...prev, [id]: true }));
    } else {
      setDisabledProviders((prev) => ({ ...prev, [id]: false }));
      setExpanded((prev) => ({ ...prev, [id]: true }));
    }
  };

  const isEnabled = (id: string, configured: boolean) => {
    if (disabledProviders[id]) return false;
    return configured || disabledProviders[id] === false;
  };

  const onepassConfigured =
    isSensitiveSet(vault.onepassword_service_account_token) || !!vault.onepassword_vault_id;
  const bitwardenConfigured =
    !!vault.bitwarden_client_id || isSensitiveSet(vault.bitwarden_client_secret) || isSensitiveSet(vault.bitwarden_master_password) || !!vault.bitwarden_server_url;
  const hashicorpConfigured =
    !!vault.hashicorp_address || isSensitiveSet(vault.hashicorp_token) || !!vault.hashicorp_mount;
  const keepassConfigured =
    !!vault.keepass_path || isSensitiveSet(vault.keepass_password);
  const keeperConfigured = isSensitiveSet(vault.keeper_app_key);

  const testable = buildTestable(vault);

  // Reset test status to idle when vault config changes
  const prevVaultRef = useRef(vault);
  useEffect(() => {
    const prev = prevVaultRef.current;
    prevVaultRef.current = vault;

    const changed: string[] = [];
    if (vault.onepassword_service_account_token !== prev.onepassword_service_account_token || vault.onepassword_vault_id !== prev.onepassword_vault_id) changed.push("onepassword");
    if (vault.bitwarden_client_id !== prev.bitwarden_client_id || vault.bitwarden_client_secret !== prev.bitwarden_client_secret || vault.bitwarden_master_password !== prev.bitwarden_master_password || vault.bitwarden_server_url !== prev.bitwarden_server_url) changed.push("bitwarden");
    if (vault.hashicorp_address !== prev.hashicorp_address || vault.hashicorp_token !== prev.hashicorp_token || vault.hashicorp_mount !== prev.hashicorp_mount) changed.push("hashicorp");
    if (vault.keepass_path !== prev.keepass_path || vault.keepass_password !== prev.keepass_password) changed.push("keepass");
    if (vault.keeper_app_key !== prev.keeper_app_key) changed.push("keeper");

    if (changed.length > 0) {
      setTestStatuses((prev) => {
        const next = { ...prev };
        for (const id of changed) next[id] = "idle";
        return next;
      });
    }
  }, [vault]);

  // Debounced auto-test
  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);

    const testable = buildTestable(vault);
    const needsTest = testable.filter((t) => {
      const s = testStatuses[t.id];
      return !s || s === "idle";
    });
    if (needsTest.length === 0 || testingRef.current) return;

    debounceRef.current = setTimeout(() => {
      testingRef.current = true;

      setTestStatuses((prev) => {
        const next = { ...prev };
        for (const t of needsTest) next[t.id] = "testing";
        return next;
      });

      Promise.all(
        needsTest.map(async (t) => {
          try {
            await api.post("/api/vaults/test", { provider: t.provider, config: t.config });
            setTestStatuses((prev) => ({ ...prev, [t.id]: "success" }));
          } catch {
            setTestStatuses((prev) => ({ ...prev, [t.id]: "error" }));
          }
        })
      ).finally(() => {
        testingRef.current = false;
      });
    }, 800);

    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [vault, testStatuses]);

  return (
    <div className="space-y-4">
      <SectionHeader title="Vault" description="Secret management provider integrations" icon={LockClosedIcon} />

      <div className="space-y-2">
        <ProviderCard
          id="onepassword"
          name="1Password"
enabled={isEnabled("onepassword", onepassConfigured)}
          testStatus={onepassConfigured ? testStatuses.onepassword : undefined}
expanded={!!expanded.onepassword}
          onToggle={() => toggle("onepassword")}
          onEnabledChange={(v) => setProviderEnabled("onepassword", v)}
        >
          <SensitiveInput
            label="Service Account Token"
            description="1Password service account token for the `op` CLI"
            value={vault.onepassword_service_account_token}
            onChange={(onepassword_service_account_token) => onChange({ ...vault, onepassword_service_account_token })}
            placeholder="Enter service account token"
          />
          <TextInput
            label="Vault ID"
            description="Default vault identifier (optional)"
            value={vault.onepassword_vault_id}
            onChange={(onepassword_vault_id) => onChange({ ...vault, onepassword_vault_id })}
            placeholder="Enter vault ID"
          />
        </ProviderCard>

        <ProviderCard
          id="bitwarden"
          name="Bitwarden"
enabled={isEnabled("bitwarden", bitwardenConfigured)}
          testStatus={bitwardenConfigured ? testStatuses.bitwarden : undefined}
expanded={!!expanded.bitwarden}
          onToggle={() => toggle("bitwarden")}
          onEnabledChange={(v) => setProviderEnabled("bitwarden", v)}
        >
          <TextInput
            label="Client ID"
            description="Personal API key client ID"
            value={vault.bitwarden_client_id}
            onChange={(bitwarden_client_id) => onChange({ ...vault, bitwarden_client_id })}
            placeholder="Enter client ID"
          />
          <SensitiveInput
            label="Client Secret"
            description="Personal API key client secret"
            value={vault.bitwarden_client_secret}
            onChange={(bitwarden_client_secret) => onChange({ ...vault, bitwarden_client_secret })}
            placeholder="Enter client secret"
          />
          <SensitiveInput
            label="Master Password"
            description="Bitwarden master password for vault unlock"
            value={vault.bitwarden_master_password}
            onChange={(bitwarden_master_password) => onChange({ ...vault, bitwarden_master_password })}
            placeholder="Enter master password"
          />
          <TextInput
            label="Server URL"
            description="Leave empty for Bitwarden cloud"
            value={vault.bitwarden_server_url}
            onChange={(bitwarden_server_url) => onChange({ ...vault, bitwarden_server_url })}
            placeholder="https://bitwarden.example.com"
          />
        </ProviderCard>

        <ProviderCard
          id="hashicorp"
          name="HashiCorp Vault"
enabled={isEnabled("hashicorp", hashicorpConfigured)}
          testStatus={hashicorpConfigured ? testStatuses.hashicorp : undefined}
expanded={!!expanded.hashicorp}
          onToggle={() => toggle("hashicorp")}
          onEnabledChange={(v) => setProviderEnabled("hashicorp", v)}
        >
          <TextInput
            label="Address"
            description="Vault server address"
            value={vault.hashicorp_address}
            onChange={(hashicorp_address) => onChange({ ...vault, hashicorp_address })}
            placeholder="http://localhost:8200"
          />
          <SensitiveInput
            label="Token"
            description="Vault authentication token"
            value={vault.hashicorp_token}
            onChange={(hashicorp_token) => onChange({ ...vault, hashicorp_token })}
            placeholder="Enter token"
          />
          <TextInput
            label="Mount"
            description="Secrets engine mount path"
            value={vault.hashicorp_mount}
            onChange={(hashicorp_mount) => onChange({ ...vault, hashicorp_mount })}
            placeholder="secret"
          />
        </ProviderCard>

        <ProviderCard
          id="keepass"
          name="KeePass"
enabled={isEnabled("keepass", keepassConfigured)}
          testStatus={keepassConfigured ? testStatuses.keepass : undefined}
expanded={!!expanded.keepass}
          onToggle={() => toggle("keepass")}
          onEnabledChange={(v) => setProviderEnabled("keepass", v)}
        >
          <TextInput
            label="Database Path"
            description="Path to the KeePass database file"
            value={vault.keepass_path}
            onChange={(keepass_path) => onChange({ ...vault, keepass_path })}
            placeholder="/path/to/database.kdbx"
          />
          <SensitiveInput
            label="Password"
            description="KeePass database master password"
            value={vault.keepass_password}
            onChange={(keepass_password) => onChange({ ...vault, keepass_password })}
            placeholder="Enter password"
          />
        </ProviderCard>

        <ProviderCard
          id="keeper"
          name="Keeper"
enabled={isEnabled("keeper", keeperConfigured)}
          testStatus={keeperConfigured ? testStatuses.keeper : undefined}
expanded={!!expanded.keeper}
          onToggle={() => toggle("keeper")}
          onEnabledChange={(v) => setProviderEnabled("keeper", v)}
        >
          <SensitiveInput
            label="App Key"
            description="Keeper Secrets Manager application key"
            value={vault.keeper_app_key}
            onChange={(keeper_app_key) => onChange({ ...vault, keeper_app_key })}
            placeholder="Enter app key"
          />
        </ProviderCard>

        <LocalVaultPanel expanded={!!expanded.local} onToggle={() => toggle("local")} />
      </div>
    </div>
  );
}
