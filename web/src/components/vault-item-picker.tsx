"use client";

import { useEffect, useMemo, useState } from "react";
import { KeyIcon, MagnifyingGlassIcon } from "@heroicons/react/24/outline";
import { CheckIcon } from "@heroicons/react/16/solid";
import { Si1password, SiBitwarden, SiVault, SiKeepassxc } from "@icons-pack/react-simple-icons";
import { api } from "@/lib/api-client";

const PROVIDER_ICONS: Record<string, React.ComponentType<{ size?: number }>> = {
  one_password: Si1password,
  bitwarden: SiBitwarden,
  hashicorp: SiVault,
  kee_pass: SiKeepassxc,
};

export interface PickerConnection {
  id: string;
  name: string;
  provider: string;
  enabled: boolean;
}

export interface CredentialOption {
  id: string;
  name: string;
  username: string | null;
  connection_id: string;
}

export function VaultItemPicker({ connections, selection, onSelect, onSelectionResolved, initialQuery = "", isAssigned }: {
  connections: PickerConnection[];
  selection: CredentialOption | null;
  onSelect: (item: CredentialOption | null) => void;
  onSelectionResolved?: (item: CredentialOption) => void;
  initialQuery?: string;
  isAssigned?: (item: CredentialOption) => boolean;
}) {
  const enabledConns = useMemo(() => connections.filter((connection) => connection.enabled), [connections]);
  const [searchQuery, setSearchQuery] = useState(initialQuery);
  const [items, setItems] = useState<CredentialOption[]>([]);
  const [searching, setSearching] = useState(true);
  const [failedConnections, setFailedConnections] = useState<string[]>([]);
  const [searchVersion, setSearchVersion] = useState(0);

  useEffect(() => {
    const controller = new AbortController();
    setSearching(true);
    setFailedConnections([]);
    const timeout = setTimeout(async () => {
      const results = await Promise.allSettled(enabledConns.map(async (connection) => {
        const matches = await api.get<Omit<CredentialOption, "connection_id">[]>(`/api/vaults/${encodeURIComponent(connection.id)}/items?q=${encodeURIComponent(searchQuery)}`);
        return matches.map((item) => ({ ...item, connection_id: connection.id }));
      }));
      if (controller.signal.aborted) return;
      const matches = results.flatMap((result) => result.status === "fulfilled" ? result.value : []);
      matches.sort((a, b) => a.name.localeCompare(b.name) || a.connection_id.localeCompare(b.connection_id));
      setItems(matches);
      setFailedConnections(results.flatMap((result, index) => result.status === "rejected" ? [enabledConns[index].name] : []));
      setSearching(false);
    }, 300);
    return () => { controller.abort(); clearTimeout(timeout); };
  }, [enabledConns, searchQuery, searchVersion]);

  // Resolve the display name when reopening an existing credential binding.
  useEffect(() => {
    if (!selection || selection.name || !onSelectionResolved) return;
    const match = items.find((item) => item.id === selection.id && item.connection_id === selection.connection_id);
    if (match) onSelectionResolved(match);
  }, [items, selection, onSelectionResolved]);

  return (
    <div className="space-y-3">
      <div className="relative">
        <MagnifyingGlassIcon className="absolute left-2.5 top-1/2 -translate-y-1/2 h-4 w-4 text-text-tertiary" />
        <input
          type="search"
          aria-label="Search all vaults"
          autoFocus
          value={searchQuery}
          onChange={(e) => {
            setSearchQuery(e.target.value);
            onSelect(null);
            setSearching(true);
          }}
          placeholder="Search all vaults..."
          className="w-full rounded-lg border border-border bg-surface pl-8 pr-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none"
        />
      </div>
      <div>
        {enabledConns.length === 0 ? (
          <p className="text-xs text-text-tertiary py-4 text-center">No enabled vaults. Connect a vault in Settings to get started.</p>
        ) : searching ? (
          <p role="status" className="text-xs text-text-tertiary py-4 text-center">Searching all vaults...</p>
        ) : items.length > 0 ? (
          <div className="space-y-1 max-h-60 overflow-y-auto rounded-lg border border-border p-1">
            {items.map((item) => {
              const connection = enabledConns.find((connection) => connection.id === item.connection_id);
              const ProviderIcon = PROVIDER_ICONS[connection?.provider ?? ""];
              const isSelected = selection?.connection_id === item.connection_id && selection?.id === item.id;
              const alreadyGranted = isAssigned?.(item) ?? false;
              return (
                <button
                  key={JSON.stringify([item.connection_id, item.id])}
                  type="button"
                  aria-pressed={isSelected}
                  aria-label={`${item.name}, ${connection?.name}${item.username ? `, ${item.username}` : ""}${alreadyGranted ? ", already assigned" : ""}`}
                  disabled={alreadyGranted}
                  onClick={() => onSelect(item)}
                  className={`flex items-center gap-3 w-full rounded-lg border px-3 py-2.5 text-left text-sm transition ${
                    alreadyGranted
                      ? "border-border text-text-tertiary opacity-50 cursor-not-allowed"
                      : isSelected
                        ? "border-accent bg-accent/10 text-accent"
                        : "border-border text-text-secondary hover:border-accent"
                  }`}
                >
                  <span aria-hidden="true" className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-surface-tertiary text-text-secondary">
                    {ProviderIcon ? <ProviderIcon size={18} /> : <KeyIcon className="h-4 w-4" />}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate font-medium">{item.name}</span>
                    <span className="block truncate text-xs text-text-tertiary">
                      {connection?.name}{item.username ? ` · ${item.username}` : ""}
                    </span>
                  </span>
                  {alreadyGranted && <span className="text-[10px] text-text-tertiary">Already assigned</span>}
                  {isSelected && <CheckIcon className="h-4 w-4 shrink-0" />}
                </button>
              );
            })}
          </div>
        ) : (
          <p className="text-xs text-text-tertiary py-4 text-center">
            {failedConnections.length === enabledConns.length ? "Could not search your vaults." : "No credentials found across your vaults."}
          </p>
        )}
        {!searching && failedConnections.length > 0 && (
          <p role="alert" className="mt-2 text-xs text-danger">
            Could not search: {failedConnections.join(", ")}.{" "}
            <button type="button" onClick={() => setSearchVersion((version) => version + 1)} className="underline">Retry</button>
          </p>
        )}
      </div>
    </div>
  );
}
