"use client";

import { useState, useEffect, useCallback, useRef } from "react";
import type { VaultConfig, SensitiveField } from "@/lib/config-types";
import { isSensitiveSet } from "@/lib/config-types";
import { Field, TextInput, SensitiveInput, SectionHeader, HelpTip } from "@/components/settings/field";
import { ChevronRightIcon, EllipsisVerticalIcon, LockClosedIcon, PlusIcon } from "@heroicons/react/24/outline";
import { Dialog } from "@/components/dialog";
import { api } from "@/lib/api-client";
import {
  listVaultConnections,
  createVaultConnection,
  deleteVaultConnection,
  toggleVaultConnection,
  testVaultConnection,
  type VaultConnection,
  type VaultProviderType,
  type VaultConnectionConfig,
} from "@/lib/api-client";
import type { CredentialResponse } from "@/lib/types";

import { useSettings } from "@/components/settings/settings-context";
import type { TestStatus } from "@/components/settings/sections/providers-section";
import { TestStatusIcon } from "@/components/settings/sections/providers-section";

interface ServerVaultSectionProps {
  vault: VaultConfig;
  onChange: (vault: VaultConfig) => void;
}

import { Si1password, SiBitwarden, SiVault, SiKeepassxc } from "@icons-pack/react-simple-icons";

const VAULT_LOGOS: Record<string, React.ComponentType<{ className?: string; size?: number; color?: string }>> = {
  onepassword: Si1password,
  bitwarden: SiBitwarden,
  hashicorp: SiVault,
  keepass: SiKeepassxc,
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
      } catch {}
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
    } finally {
      setSubmitting(false);
    }
   
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
  | { type: "KeePass"; file_path: string; master_password: string };

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

  return result;
}

const VAULT_CLEAR_FIELDS: Record<string, Partial<VaultConfig>> = {
  onepassword: { onepassword_service_account_token: { is_set: false }, onepassword_vault_id: null },
  bitwarden: { bitwarden_client_id: null, bitwarden_client_secret: { is_set: false }, bitwarden_master_password: { is_set: false }, bitwarden_server_url: null },
  hashicorp: { hashicorp_address: null, hashicorp_token: { is_set: false }, hashicorp_mount: null },
  keepass: { keepass_path: null, keepass_password: { is_set: false } },
};

type ProviderId = "managed" | "onepassword" | "bitwarden" | "hashicorp" | "keepass";

const PROVIDER_TYPE_TO_ID: Record<VaultProviderType, ProviderId | "local" | "managed"> = {
  one_password: "onepassword",
  bitwarden: "bitwarden",
  hashicorp: "hashicorp",
  kee_pass: "keepass",
  local: "local",
  managed: "managed",
};

const PROVIDER_OPTIONS: { id: ProviderId; name: string; provider: VaultProviderType }[] = [
  { id: "managed", name: "Managed", provider: "managed" },
  { id: "onepassword", name: "1Password", provider: "one_password" },
  { id: "bitwarden", name: "Bitwarden", provider: "bitwarden" },
  { id: "hashicorp", name: "HashiCorp Vault", provider: "hashicorp" },
  { id: "keepass", name: "KeePass", provider: "kee_pass" },
];

interface DraftConnection {
  providerId: ProviderId;
  provider: VaultProviderType;
  name: string;
  onepassword_token?: string;
  onepassword_vault_id?: string;
  bitwarden_client_id?: string;
  bitwarden_client_secret?: string;
  bitwarden_master_password?: string;
  bitwarden_server_url?: string;
  hashicorp_address?: string;
  hashicorp_token?: string;
  hashicorp_mount?: string;
  keepass_path?: string;
  keepass_password?: string;
}

function buildDraftConfig(draft: DraftConnection): VaultConnectionConfig | null {
  switch (draft.providerId) {
    case "managed":
      return { type: "Managed" };
    case "onepassword":
      if (!draft.onepassword_token?.trim()) return null;
      return {
        type: "OnePassword",
        service_account_token: draft.onepassword_token,
        default_vault_id: draft.onepassword_vault_id?.trim() || null,
      };
    case "bitwarden":
      if (!draft.bitwarden_client_id?.trim() || !draft.bitwarden_client_secret?.trim() || !draft.bitwarden_master_password?.trim()) return null;
      return {
        type: "Bitwarden",
        client_id: draft.bitwarden_client_id,
        client_secret: draft.bitwarden_client_secret,
        master_password: draft.bitwarden_master_password,
        server_url: draft.bitwarden_server_url?.trim() || null,
      };
    case "hashicorp":
      if (!draft.hashicorp_address?.trim() || !draft.hashicorp_token?.trim()) return null;
      return {
        type: "Hashicorp",
        address: draft.hashicorp_address,
        token: draft.hashicorp_token,
        mount_path: draft.hashicorp_mount?.trim() || null,
      };
    case "keepass":
      if (!draft.keepass_path?.trim() || !draft.keepass_password?.trim()) return null;
      return {
        type: "KeePass",
        file_path: draft.keepass_path,
        master_password: draft.keepass_password,
      };
  }
}

function ConnectionRow({ connection, onDelete, onToggle, onTest }: {
  connection: VaultConnection;
  onDelete: () => void;
  onToggle: (enabled: boolean) => void;
  onTest: () => Promise<void>;
}) {
  const providerId = PROVIDER_TYPE_TO_ID[connection.provider];
  const Logo = providerId !== "local" && providerId !== "managed" ? VAULT_LOGOS[providerId] : undefined;
  const [menuOpen, setMenuOpen] = useState(false);
  const [testStatus, setTestStatus] = useState<TestStatus>("idle");
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false);
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [menuOpen]);

  const runTest = async () => {
    setTestStatus("testing");
    try {
      await onTest();
      setTestStatus("success");
    } catch {
      setTestStatus("error");
    }
  };

  return (
    <div className="flex items-center justify-between rounded-lg border border-border bg-surface-secondary px-4 py-3">
      <div className="flex flex-1 items-center gap-2.5">
        {Logo && <Logo size={18} className="text-text-tertiary" />}
        <span className="text-sm font-medium text-text-primary">{connection.name}</span>
        {testStatus !== "idle" && <TestStatusIcon status={testStatus} />}
      </div>
      <div className="flex items-center gap-2">
        <button
          type="button"
          aria-label={`${connection.enabled ? "Disable" : "Enable"} ${connection.name}`}
          onClick={() => onToggle(!connection.enabled)}
          className={`relative inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors ${connection.enabled ? "bg-accent" : "bg-surface-tertiary"}`}
        >
          <span className={`pointer-events-none inline-block h-5 w-5 rounded-full bg-surface shadow transform transition-transform ${connection.enabled ? "translate-x-5" : "translate-x-0"}`} />
        </button>
        <div className="relative" ref={menuRef}>
          <button
            type="button"
            aria-label={`Actions for ${connection.name}`}
            onClick={() => setMenuOpen((v) => !v)}
            className="p-1 rounded text-text-tertiary hover:text-text-primary hover:bg-surface-tertiary transition"
          >
            <EllipsisVerticalIcon className="h-4 w-4" />
          </button>
          {menuOpen && (
            <div className="absolute right-0 top-full mt-1 w-32 bg-surface border border-border rounded-lg shadow-lg z-10">
              <button
                type="button"
                onClick={() => { setMenuOpen(false); runTest(); }}
                className="w-full text-left px-3 py-2 text-sm text-text-secondary hover:bg-surface-tertiary transition rounded-t-lg"
              >
                Test
              </button>
              <button
                type="button"
                onClick={() => { setMenuOpen(false); onDelete(); }}
                className="w-full text-left px-3 py-2 text-sm text-danger hover:bg-surface-tertiary transition rounded-b-lg"
              >
                Delete
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function defaultNameFor(providerId: ProviderId): string {
  return `My ${PROVIDER_OPTIONS.find((o) => o.id === providerId)!.name}`;
}

function AddConnectionDialog({ open, onClose, onCreated }: { open: boolean; onClose: () => void; onCreated: () => void }) {
  const [step, setStep] = useState<"pick" | "configure">("pick");
  const [draft, setDraft] = useState<DraftConnection | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [testStatus, setTestStatus] = useState<TestStatus>("idle");
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    if (open) {
      setStep("pick");
      setDraft(null);
      setError(null);
      setSubmitting(false);
      setTestStatus("idle");
    }
  }, [open]);

  useEffect(() => {
    if (debounceRef.current) clearTimeout(debounceRef.current);
    if (!draft) {
      setTestStatus("idle");
      return;
    }
    const config = buildDraftConfig(draft);
    if (!config) {
      setTestStatus("idle");
      return;
    }
    setTestStatus("testing");
    debounceRef.current = setTimeout(async () => {
      try {
        await api.post("/api/vaults/test", { provider: draft.provider, config });
        setTestStatus("success");
      } catch {
        setTestStatus("error");
      }
    }, 800);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [draft]);

  const update = (patch: Partial<DraftConnection>) => setDraft((prev) => prev ? { ...prev, ...patch } : prev);

  const pickProvider = (providerId: ProviderId) => {
    const option = PROVIDER_OPTIONS.find((o) => o.id === providerId)!;
    setDraft({ providerId, provider: option.provider, name: defaultNameFor(providerId) });
    setStep("configure");
    setError(null);
  };

  const canCreate = !!draft && draft.name.trim().length > 0 && testStatus === "success";

  const handleCreate = async () => {
    if (!draft) return;
    const config = buildDraftConfig(draft);
    if (!draft.name.trim()) {
      setError("Name is required");
      return;
    }
    if (!config) {
      setError("All required fields must be filled in");
      return;
    }
    if (testStatus !== "success") {
      setError("The connection could not be verified — fix the credentials and try again.");
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await createVaultConnection({ name: draft.name, provider: draft.provider, config });
      onCreated();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create connection");
    } finally {
      setSubmitting(false);
    }
  };

  const selectedOption = draft ? PROVIDER_OPTIONS.find((o) => o.id === draft.providerId) : undefined;
  const title = step === "pick"
    ? "Add connection"
    : `New ${selectedOption?.name ?? ""} connection`;
  const description = step === "pick"
    ? "Choose a vault provider to connect."
    : draft?.providerId === "managed"
      ? "Create a personal vault for credentials managed by Frona."
      : "Enter the credentials this connection should use.";
  const headerIcon = step === "configure" && selectedOption && selectedOption.id !== "managed"
    ? VAULT_LOGOS[selectedOption.id] as React.ComponentType<{ className?: string }> | undefined
    : LockClosedIcon;

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={title}
      description={description}
      icon={headerIcon}
      onBack={step === "configure" ? () => { setStep("pick"); setError(null); } : undefined}
    >
      {step === "pick" && (
        <div className="space-y-2">
          {PROVIDER_OPTIONS.map((opt) => {
            const Logo = opt.id === "managed" ? undefined : VAULT_LOGOS[opt.id];
            return (
              <button
                key={opt.id}
                type="button"
                onClick={() => pickProvider(opt.id)}
                className="w-full flex items-center gap-3 rounded-lg border border-border px-4 py-3 text-left transition hover:bg-surface-tertiary"
              >
                {Logo && <Logo size={22} className="text-text-secondary shrink-0" />}
                <span className="text-sm font-medium text-text-primary flex-1">{opt.name}</span>
                <ChevronRightIcon className="h-4 w-4 text-text-tertiary" />
              </button>
            );
          })}
        </div>
      )}

      {step === "configure" && draft && (
        <div className="space-y-3">
              <TextInput
                label="Name"
                description="A label to recognize this connection"
                value={draft.name}
                onChange={(name) => update({ name })}
                placeholder={defaultNameFor(draft.providerId)}
              />

              {draft.providerId === "onepassword" && (
        <>
          <TextInput
            label="Service Account Token"
            description="1Password service account token (used by the `op` CLI)"
            value={draft.onepassword_token ?? ""}
            onChange={(v) => update({ onepassword_token: v })}
            placeholder="ops_..."
            type="password"
          />
          <TextInput
            label="Default Vault ID"
            description="Default vault identifier — optional"
            value={draft.onepassword_vault_id ?? ""}
            onChange={(v) => update({ onepassword_vault_id: v })}
            placeholder="Vault identifier"
          />
        </>
      )}

      {draft.providerId === "bitwarden" && (
        <>
          <TextInput
            label="Client ID"
            description="Personal API key client ID"
            value={draft.bitwarden_client_id ?? ""}
            onChange={(v) => update({ bitwarden_client_id: v })}
            placeholder="Client ID"
          />
          <TextInput
            label="Client Secret"
            description="Personal API key client secret"
            value={draft.bitwarden_client_secret ?? ""}
            onChange={(v) => update({ bitwarden_client_secret: v })}
            placeholder="Client secret"
            type="password"
          />
          <TextInput
            label="Master Password"
            description="Vault unlock password"
            value={draft.bitwarden_master_password ?? ""}
            onChange={(v) => update({ bitwarden_master_password: v })}
            placeholder="Master password"
            type="password"
          />
          <TextInput
            label="Server URL"
            description="Leave empty for Bitwarden cloud"
            value={draft.bitwarden_server_url ?? ""}
            onChange={(v) => update({ bitwarden_server_url: v })}
            placeholder="https://bitwarden.example.com"
          />
        </>
      )}

      {draft.providerId === "hashicorp" && (
        <>
          <TextInput
            label="Address"
            description="Vault server address"
            value={draft.hashicorp_address ?? ""}
            onChange={(v) => update({ hashicorp_address: v })}
            placeholder="https://vault.example.com"
          />
          <TextInput
            label="Token"
            description="Vault authentication token"
            value={draft.hashicorp_token ?? ""}
            onChange={(v) => update({ hashicorp_token: v })}
            placeholder="Authentication token"
            type="password"
          />
          <TextInput
            label="Mount Path"
            description="Secrets engine mount path (e.g. secret)"
            value={draft.hashicorp_mount ?? ""}
            onChange={(v) => update({ hashicorp_mount: v })}
            placeholder="secret"
          />
        </>
      )}

              {draft.providerId === "keepass" && (
                <>
                  <TextInput
                    label="Database Path"
                    description="Path to the KeePass database file"
                    value={draft.keepass_path ?? ""}
                    onChange={(v) => update({ keepass_path: v })}
                    placeholder="/path/to/database.kdbx"
                  />
                  <TextInput
                    label="Master Password"
                    description="KeePass database master password"
                    value={draft.keepass_password ?? ""}
                    onChange={(v) => update({ keepass_password: v })}
                    placeholder="Master password"
                    type="password"
                  />
                </>
              )}

          {error && <p className="text-xs text-error-text">{error}</p>}

          <div className="flex items-center gap-3 pt-4">
            <button
              type="button"
              onClick={handleCreate}
              disabled={submitting || !canCreate}
              className="w-32 inline-flex items-center justify-center gap-1.5 rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover disabled:opacity-50 transition"
            >
              {submitting ? "Creating..." : "Create"}
            </button>
            <button
              type="button"
              onClick={onClose}
              className="w-32 inline-flex items-center justify-center gap-1.5 rounded-lg border border-border px-4 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary transition"
            >
              Cancel
            </button>
            {testStatus !== "idle" && (
              <div className="flex items-center gap-1.5 text-xs text-text-tertiary">
                <TestStatusIcon status={testStatus} />
                {testStatus === "testing" && "Testing…"}
                {testStatus === "success" && "Verified"}
                {testStatus === "error" && <span className="text-error-text">Could not connect</span>}
              </div>
            )}
          </div>
        </div>
      )}
    </Dialog>
  );
}

function PersonalConnectionsPanel() {
  const [error, setError] = useState<string | null>(null);
  const [connections, setConnections] = useState<VaultConnection[]>([]);
  const [loading, setLoading] = useState(true);
  const [dialogOpen, setDialogOpen] = useState(false);

  const reload = useCallback(async () => {
    try {
      const all = await listVaultConnections();
      setConnections(all.filter((c) => !c.system_managed));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { reload(); }, [reload]);

  const handleDelete = async (id: string) => {
    if (!confirm("Delete this connection? Managed vaults must be empty first.")) return;
    setError(null);
    try {
      await deleteVaultConnection(id);
      await reload();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to delete connection");
    }
  };

  const handleToggle = async (id: string, enabled: boolean) => {
    try {
      await toggleVaultConnection(id, enabled);
      await reload();
    } catch {}
  };

  return (
    <div className="space-y-2">
      {error && <p role="alert" className="text-sm text-error-text">{error}</p>}
      {loading ? (
        <div className="flex items-center justify-center py-4">
          <svg className="h-4 w-4 animate-spin text-text-tertiary" viewBox="0 0 24 24" fill="none">
            <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
            <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
          </svg>
        </div>
      ) : (
        <>
          {connections.length === 0 && (
            <p className="text-sm text-text-tertiary px-2 py-3">No connections yet.</p>
          )}
          {connections.map((c) => (
            <ConnectionRow
              key={c.id}
              connection={c}
              onDelete={() => handleDelete(c.id)}
              onToggle={(enabled) => handleToggle(c.id, enabled)}
              onTest={() => testVaultConnection(c.id)}
            />
          ))}
          <button
            type="button"
            onClick={() => setDialogOpen(true)}
            className="w-full flex items-center justify-center gap-1.5 rounded-lg border border-dashed border-border px-3 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
          >
            <PlusIcon className="h-4 w-4" />
            Add connection
          </button>
          <AddConnectionDialog
            open={dialogOpen}
            onClose={() => setDialogOpen(false)}
            onCreated={() => { setDialogOpen(false); reload(); }}
          />
        </>
      )}
    </div>
  );
}

export function UserVaultSection() {
  return (
    <div className="space-y-6">
      <SectionHeader title="Vault" description="Your personal credential store. Agents use these to log in, authenticate APIs, or access protected services on your behalf." icon={LockClosedIcon} />
      <div className="space-y-2">
        <h3 className="text-sm font-semibold text-text-primary px-1">Connections</h3>
        <PersonalConnectionsPanel />
      </div>
      <div className="space-y-2">
        <h3 className="text-sm font-semibold text-text-primary px-1">Local</h3>
        <LocalVaultPanel expanded onToggle={() => {}} />
      </div>
    </div>
  );
}

export function ServerVaultSection({ vault, onChange }: ServerVaultSectionProps) {
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

  const testable = buildTestable(vault);

  // Reset test status to idle when vault config changes
  const [prevVault, setPrevVault] = useState(vault);
  if (vault !== prevVault) {
    setPrevVault(vault);
    const changed: string[] = [];
    if (vault.onepassword_service_account_token !== prevVault.onepassword_service_account_token || vault.onepassword_vault_id !== prevVault.onepassword_vault_id) changed.push("onepassword");
    if (vault.bitwarden_client_id !== prevVault.bitwarden_client_id || vault.bitwarden_client_secret !== prevVault.bitwarden_client_secret || vault.bitwarden_master_password !== prevVault.bitwarden_master_password || vault.bitwarden_server_url !== prevVault.bitwarden_server_url) changed.push("bitwarden");
    if (vault.hashicorp_address !== prevVault.hashicorp_address || vault.hashicorp_token !== prevVault.hashicorp_token || vault.hashicorp_mount !== prevVault.hashicorp_mount) changed.push("hashicorp");
    if (vault.keepass_path !== prevVault.keepass_path || vault.keepass_password !== prevVault.keepass_password) changed.push("keepass");
    if (changed.length > 0) {
      setTestStatuses((prev) => {
        const next = { ...prev };
        for (const id of changed) next[id] = "idle";
        return next;
      });
    }
  }

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
     
  }, [vault, testStatuses]);

  return (
    <div className="space-y-4">
      <SectionHeader title="Vault" description="Server-wide credential providers. Connections configured here are available to every user. Changes take effect after restart." icon={LockClosedIcon} />

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
            description="1Password service account token (used by the `op` CLI)"
            value={vault.onepassword_service_account_token}
            onChange={(onepassword_service_account_token) => onChange({ ...vault, onepassword_service_account_token })}
            placeholder="ops_..."
          />
          <TextInput
            label="Default Vault ID"
            description="Default vault identifier — optional"
            value={vault.onepassword_vault_id}
            onChange={(onepassword_vault_id) => onChange({ ...vault, onepassword_vault_id })}
            placeholder="Vault identifier"
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
            placeholder="Client ID"
          />
          <SensitiveInput
            label="Client Secret"
            description="Personal API key client secret"
            value={vault.bitwarden_client_secret}
            onChange={(bitwarden_client_secret) => onChange({ ...vault, bitwarden_client_secret })}
            placeholder="Client secret"
          />
          <SensitiveInput
            label="Master Password"
            description="Vault unlock password"
            value={vault.bitwarden_master_password}
            onChange={(bitwarden_master_password) => onChange({ ...vault, bitwarden_master_password })}
            placeholder="Master password"
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
            placeholder="https://vault.example.com"
          />
          <SensitiveInput
            label="Token"
            description="Vault authentication token"
            value={vault.hashicorp_token}
            onChange={(hashicorp_token) => onChange({ ...vault, hashicorp_token })}
            placeholder="Authentication token"
          />
          <TextInput
            label="Mount Path"
            description="Secrets engine mount path (e.g. secret)"
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
            label="Master Password"
            description="KeePass database master password"
            value={vault.keepass_password}
            onChange={(keepass_password) => onChange({ ...vault, keepass_password })}
            placeholder="Master password"
          />
        </ProviderCard>

      </div>
    </div>
  );
}
