"use client";

import { ConfigurationSidebar } from "@/components/layout/configuration-sidebar";

import { useState, useEffect, useCallback, Suspense } from "react";
import { useSearchParams, useRouter } from "next/navigation";
import {
  ChatBubbleLeftRightIcon,
  PlayIcon,
  StopIcon,
  TrashIcon,
  InformationCircleIcon,
  Cog6ToothIcon,
  KeyIcon,
  ClipboardDocumentIcon,
  CheckIcon,
} from "@heroicons/react/24/outline";
import { api } from "@/lib/api-client";
import { sseBus } from "@/lib/sse-event-bus";
import { SectionHeader, SectionPanel } from "@/components/settings/field";
import { AddCredentialForm } from "@/components/agents/configure/creds-section";
import type { VaultGrant, VaultConnection, PendingCredential } from "@/components/agents/configure/creds-section";
import { ManifestInfo, MarkdownProse, type ExternalLink } from "@/components/channels/manifest-info";
import { DeleteConfirmDialog } from "@/components/nav/delete-confirm-dialog";
import { QRCodeSVG } from "qrcode.react";
import { formatDistanceToNow } from "date-fns";
import type { Agent, SpaceResponse } from "@/lib/types";

interface UserAddress {
  address: string | null;
  pairing_code: string | null;
  pairing_initiated_at: string | null;
  paired_at: string | null;
}

interface SetupConfig {
  code: string | null;
  qr: string | null;
  instructions: string | null;
  expires_at: string | null;
  initiated_at: string | null;
}

interface RetryConfig {
  max_retries: number;
  initial_backoff_ms: number;
  backoff_multiplier: number;
  max_backoff_ms: number;
}

interface Channel {
  id: string;
  user_id: string;
  space_id: string;
  provider: string;
  agent_id: string;
  config: Record<string, string>;
  dispatch_mode: "message" | "signal";
  // Connection status only; setup/pairing are overlays (`setup`, `user_address.pairing_code`).
  status: "disconnected" | "connecting" | "connected" | "reconnecting" | "failed";
  enabled: boolean;
  error_message: string | null;
  last_started_at: string | null;
  user_address: UserAddress | null;
  setup: SetupConfig | null;
  retry: RetryConfig | null;
  created_at: string;
  updated_at: string;
  webhook_url?: string;
}

interface ChannelConfigField {
  name: string;
  description: string | null;
  is_required: boolean;
  is_secret: boolean;
  format: string | null;
  // For secret fields the resolved value is stripped server-side; presence of
  // `default_from` is the only signal that a server-config fallback exists.
  default_from: { section: string; field: string } | null;
  default_resolved: string | null;
}

const DISPATCH_MODE_LABEL: Record<string, string> = {
  message: "Process as your message",
  signal: "Hand off to a waiting agent",
};

const RETRY_FOREVER = 4294967295;
const CHANNEL_RETRY_DEFAULTS: RetryConfig = {
  max_retries: RETRY_FOREVER,
  initial_backoff_ms: 1000,
  backoff_multiplier: 2.0,
  max_backoff_ms: 60000,
};

function RetryNumberInput({
  label,
  value,
  placeholder,
  step,
  forever,
  onChange,
}: {
  label: string;
  value: number | undefined;
  placeholder?: string;
  step?: number;
  forever?: boolean;
  onChange: (v: number | null) => void;
}) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-xs text-text-secondary">{label}</span>
      <input
        type="number"
        min={0}
        step={step ?? 1}
        value={forever ? "" : (value ?? "")}
        placeholder={forever ? "Forever" : placeholder}
        onChange={(e) => {
          const raw = e.target.value;
          if (raw === "") {
            onChange(null);
          } else {
            const n = Number(raw);
            onChange(Number.isFinite(n) ? n : null);
          }
        }}
        className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none font-mono"
      />
    </label>
  );
}

interface ChannelManifest {
  id: string;
  display_name: string;
  description: string;
  config_fields: ChannelConfigField[];
  webhook_url_visible?: boolean;
  setup_instructions?: string | null;
  external_links?: ExternalLink[];
}

const SECTIONS = [
  { id: "status", label: "Status" },
  { id: "config", label: "Config" },
  { id: "about", label: "About" },
] as const;

type SectionId = (typeof SECTIONS)[number]["id"];

// Keyed by the *derived* badge label (connection status + overlays) — see `channelBadge`.
const STATUS_BADGE: Record<string, string> = {
  disconnected: "bg-surface-tertiary text-text-secondary",
  connecting: "bg-blue-400/15 text-blue-400",
  connected: "bg-green-500/15 text-green-500",
  reconnecting: "bg-amber-500/15 text-amber-400",
  failed: "bg-red-500/15 text-red-500",
  pairing: "bg-purple-500/15 text-purple-400",
  linking: "bg-yellow-500/15 text-yellow-500",
};

/// Pending QR (setup) or pairing code takes visual priority over the connection status.
function channelBadge(c: Channel): string {
  if (c.setup) return "linking";
  if (c.user_address?.pairing_code) return "pairing";
  return c.status;
}

// Green is reserved for the `connected` status badge — keep it out of the
// provider palette so the two don't collide visually on a row.
const PROVIDER_COLORS = [
  "bg-blue-500/15 text-blue-400",
  "bg-indigo-500/15 text-indigo-400",
  "bg-purple-500/15 text-purple-400",
  "bg-pink-500/15 text-pink-400",
  "bg-orange-500/15 text-orange-400",
  "bg-cyan-500/15 text-cyan-400",
];

function providerBadgeClass(providerId: string, manifests: ChannelManifest[]): string {
  const idx = manifests.findIndex((m) => m.id === providerId);
  if (idx < 0) return "bg-surface-tertiary text-text-tertiary";
  return PROVIDER_COLORS[idx % PROVIDER_COLORS.length];
}

function WebhookUrlField({ url }: { url: string }) {
  const [copied, setCopied] = useState(false);
  const handleCopy = () => {
    navigator.clipboard.writeText(url);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };
  return (
    <div className="relative">
      <input
        readOnly
        value={url}
        onFocus={(e) => e.currentTarget.select()}
        className="w-full rounded-lg border border-border bg-surface pl-3 pr-10 py-2 text-sm font-mono text-text-primary focus:border-accent focus:outline-none"
      />
      <button
        onClick={handleCopy}
        aria-label={copied ? "Copied" : "Copy webhook URL"}
        className="absolute top-1/2 -translate-y-1/2 right-1.5 flex items-center justify-center h-7 w-7 rounded-md text-text-secondary hover:text-text-primary hover:bg-surface-tertiary transition"
      >
        {copied ? (
          <CheckIcon className="h-4 w-4 text-[#4fd1c5]" />
        ) : (
          <ClipboardDocumentIcon className="h-4 w-4" />
        )}
      </button>
    </div>
  );
}

function ChannelDetailPage() {
  const searchParams = useSearchParams();
  const router = useRouter();
  const channelId = searchParams.get("id");

  const [channel, setChannel] = useState<Channel | null>(null);
  const [manifest, setManifest] = useState<ChannelManifest | null>(null);
  const [allManifests, setAllManifests] = useState<ChannelManifest[]>([]);
  const [space, setSpace] = useState<SpaceResponse | null>(null);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [actionLoading, setActionLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [confirmDeleteOpen, setConfirmDeleteOpen] = useState(false);
  const [deleteAssociatedSpace, setDeleteAssociatedSpace] = useState(false);

  const [agentId, setAgentId] = useState("");
  const [dispatchMode, setDispatchMode] = useState<"message" | "signal">("message");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [retry, setRetry] = useState<RetryConfig | null>(null);

  // Picker writes are staged locally; the page Save button commits pending
  // grants and deleted grants alongside config changes so the swap is atomic.
  const [connections, setConnections] = useState<Map<string, VaultConnection>>(new Map());
  const [grants, setGrants] = useState<VaultGrant[]>([]);
  const [pendingCreds, setPendingCreds] = useState<PendingCredential[]>([]);
  const [deletedGrantIds, setDeletedGrantIds] = useState<Set<string>>(new Set());
  const [credDialogEnvVar, setCredDialogEnvVar] = useState<string | null>(null);
  const [credDialogExisting, setCredDialogExisting] = useState<
    { connection_id: string; vault_item_id: string } | undefined
  >(undefined);

  const sectionParam = searchParams.get("section");
  const initialSection = SECTIONS.some((s) => s.id === sectionParam)
    ? (sectionParam as SectionId)
    : "status";
  const [activeSection, setActiveSectionState] = useState<SectionId>(initialSection);

  const setActiveSection = useCallback(
    (section: SectionId) => {
      setActiveSectionState(section);
      const params = new URLSearchParams(searchParams.toString());
      params.set("section", section);
      router.replace(`/channels?${params.toString()}`, { scroll: false });
    },
    [router, searchParams],
  );

  const reload = useCallback(async () => {
    if (!channelId) return;
    try {
      const [c, mans, ags, conns, allGrants, spcs] = await Promise.all([
        api.get<Channel>(`/api/channels/${channelId}`),
        api.get<ChannelManifest[]>("/api/channels/manifests"),
        api.get<Agent[]>("/api/agents"),
        api.get<VaultConnection[]>("/api/vaults"),
        api.get<VaultGrant[]>("/api/vaults/grants"),
        api.get<SpaceResponse[]>("/api/spaces"),
      ]);
      setChannel(c);
      setManifest(mans.find((m) => m.id === c.provider) ?? null);
      setAllManifests(mans);
      setSpace(spcs.find((s) => s.id === c.space_id) ?? null);
      setAgents(ags);
      setAgentId(c.agent_id);
      setDispatchMode(c.dispatch_mode);
      setConfig(c.config ?? {});
      setRetry(c.retry);
      setConnections(new Map(conns.map((cn) => [cn.id, cn])));
      setGrants(
        allGrants.filter(
          (g) => g.principal?.kind === "channel" && g.principal?.id === channelId,
        ),
      );
      setPendingCreds([]);
      setDeletedGrantIds(new Set());
      setDirty(false);
    } catch {
      setError("Failed to load channel");
    } finally {
      setLoading(false);
    }
  }, [channelId]);

  useEffect(() => {
    reload();
  }, [reload]);

  // Hide Config only when there's literally nothing to show - no fields
  // AND no webhook URL.
  const configTabHasContent = (m: ChannelManifest) =>
    m.config_fields.length > 0 || !!m.webhook_url_visible;
  const visibleSections = SECTIONS.filter((s) => {
    if (s.id === "config" && manifest && !configTabHasContent(manifest)) {
      return false;
    }
    return true;
  });

  // Don't redirect off Config during `setup` with a non-empty config -
  // that's exactly when the user needs to be there to fill the fields.
  useEffect(() => {
    if (activeSection !== "config" || !channel || !manifest) return;
    const tabHidden = !configTabHasContent(manifest);
    if (tabHidden || channel.user_address?.pairing_code != null) {
      setActiveSection("status");
    }
  }, [activeSection, channel, manifest, setActiveSection]);

  const save = async () => {
    if (!channel) return;
    setSaving(true);
    setError(null);
    try {
      // Apply credential changes before the config PATCH — the PATCH triggers
      // a channel restart that must observe the final binding set.
      for (const id of deletedGrantIds) {
        await api.delete(`/api/vaults/grants/${id}`);
      }
      for (const cred of pendingCreds) {
        await api.post("/api/vaults/grants", cred);
      }

      const patch: Record<string, unknown> = {};
      if (agentId !== channel.agent_id) patch.agent_id = agentId;
      if (dispatchMode !== channel.dispatch_mode) patch.dispatch_mode = dispatchMode;
      const cfgChanged = JSON.stringify(config) !== JSON.stringify(channel.config ?? {});
      if (cfgChanged) patch.config = config;
      const retryChanged = JSON.stringify(retry) !== JSON.stringify(channel.retry);
      if (retryChanged) patch.retry = retry;
      if (Object.keys(patch).length > 0) {
        await api.patch(`/api/channels/${channelId}`, patch);
      }
      await reload();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Save failed");
    } finally {
      setSaving(false);
    }
  };

  const start = async () => {
    setActionLoading(true);
    setError(null);
    try {
      await api.post(`/api/channels/${channelId}/start`, {});
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Start failed");
    } finally {
      await reload();
      setActionLoading(false);
    }
  };

  const stop = async () => {
    setActionLoading(true);
    try {
      await api.post(`/api/channels/${channelId}/stop`, {});
      await reload();
    } catch {
    } finally {
      setActionLoading(false);
    }
  };

  const remove = async () => {
    setActionLoading(true);
    setError(null);
    try {
      const query = deleteAssociatedSpace ? "?delete_space=true" : "";
      await api.delete(`/api/channels/${channelId}${query}`);
      setConfirmDeleteOpen(false);
      setDeleteAssociatedSpace(false);
      router.push("/settings#channels");
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Delete failed");
    } finally {
      setActionLoading(false);
    }
  };

  const initiatePairing = async () => {
    setActionLoading(true);
    setError(null);
    try {
      await api.post(`/api/channels/${channelId}/pair`, {});
      await reload();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Pairing failed");
    } finally {
      setActionLoading(false);
    }
  };

  const cancelPairing = async () => {
    setActionLoading(true);
    try {
      await api.delete(`/api/channels/${channelId}/pair`);
      await reload();
    } catch {
    } finally {
      setActionLoading(false);
    }
  };

  useEffect(() => {
    if (!channelId) return;
    const unsub = sseBus.onGlobal((event) => {
      if (event.type === "entity_updated"
          && event.table === "channel"
          && event.recordId === channelId) {
        reload();
      }
    });
    return unsub;
  }, [channelId, reload]);

  if (!channelId) {
    return <p className="p-8 text-sm text-error-text">No channel ID provided.</p>;
  }
  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="h-5 w-5 animate-spin rounded-full border-2 border-accent border-t-transparent" />
      </div>
    );
  }
  if (!channel) {
    return <p className="p-8 text-sm text-error-text">{error || "Channel not found"}</p>;
  }

  // Intent-driven: ▶ enables (or retries a terminally-failed channel), ⏸ disables.
  const canStart = !channel.enabled || channel.status === "failed";
  const canStop = channel.enabled;
  const startLabel = channel.enabled ? "Retry" : "Enable";
  const statusBadge = channelBadge(channel);
  const displayName = space?.name ?? manifest?.display_name ?? channel.provider;

  return (
    <div className="flex h-full min-h-0 min-w-0 bg-surface">
      <DeleteConfirmDialog
        open={confirmDeleteOpen}
        onCancel={() => {
          setConfirmDeleteOpen(false);
          setDeleteAssociatedSpace(false);
        }}
        onConfirm={remove}
        title="Delete channel?"
        message={
          <div className="space-y-3">
            <p>
              This will permanently delete the channel. Pending messages will not be sent.
            </p>
            <label className="flex items-start gap-2 rounded-lg border border-border bg-surface-secondary p-3">
              <input
                type="checkbox"
                checked={deleteAssociatedSpace}
                onChange={(e) => setDeleteAssociatedSpace(e.target.checked)}
                className="mt-0.5"
              />
              <span>
                Also delete the associated space{space ? ` "${space.name}"` : ""} and all of its chats and messages.
              </span>
            </label>
            {deleteAssociatedSpace && (space?.chat_count ?? 0) > 0 && (
              <div className="rounded-lg border border-danger/30 bg-danger/10 px-3 py-2 font-medium text-danger">
                {space?.chat_count} {space?.chat_count === 1 ? "chat" : "chats"} will be deleted
              </div>
            )}
          </div>
        }
        confirmLabel="Delete channel"
        confirming={actionLoading}
        confirmingLabel="Deleting..."
      />
      <ConfigurationSidebar
        backHref="/settings#channels"
        backLabel="Back to Channels"
        sections={visibleSections}
        activeSection={activeSection}
        onSectionChange={setActiveSection}
        header={
          <div className="flex items-center gap-2 mb-4">
            <ChatBubbleLeftRightIcon className="h-8 w-8 text-text-tertiary shrink-0" />
            <div className="min-w-0">
              <h2 className="text-sm font-semibold text-text-primary truncate">{displayName}</h2>
              <div className="flex items-center gap-1 flex-wrap mt-0.5">
                <span
                  className={`inline-block rounded-full px-2 py-0.5 text-[10px] font-medium ${providerBadgeClass(channel.provider, allManifests)}`}
                >
                  {manifest?.display_name ?? channel.provider}
                </span>
                <span
                  className={`inline-block rounded-full px-2 py-0.5 text-[10px] font-medium ${
                    STATUS_BADGE[statusBadge] ?? STATUS_BADGE.disconnected
                  }`}
                >
                  {statusBadge}
                </span>
              </div>
            </div>
          </div>
        }
      />

      <div className="flex-1 min-h-0 min-w-0 overflow-y-auto [overflow-wrap:anywhere]">
        <div className="max-w-2xl mx-auto p-4 md:p-8 space-y-6">
          {activeSection === "about" && manifest && (
            <div className="space-y-6">
              <SectionHeader
                title="About"
                description="What this provider is and how to set it up."
                icon={InformationCircleIcon}
              />
              <SectionPanel title={manifest.display_name}>
                <ManifestInfo manifest={manifest} />
              </SectionPanel>
              {channel.setup != null && manifest.setup_instructions && (
                <SectionPanel title="Setup">
                  <MarkdownProse source={manifest.setup_instructions} />
                </SectionPanel>
              )}
            </div>
          )}

          {activeSection === "status" && (
            <div className="space-y-6">
              <SectionHeader
                title="Status"
                description="Connection state and lifecycle controls"
                icon={InformationCircleIcon}
              />
              {error && (
                <div className="rounded-lg border border-red-500/30 bg-red-500/10 px-4 py-3 text-sm text-red-400">
                  {error}
                </div>
              )}
              <div className="rounded-xl border border-border bg-surface-secondary divide-y divide-border overflow-hidden">
                <div className="px-4 py-3 flex justify-between">
                  <span className="text-sm text-text-tertiary">Provider</span>
                  <span className="text-sm text-text-primary font-mono">{channel.provider}</span>
                </div>
                <div className="px-4 py-3 flex justify-between">
                  <span className="text-sm text-text-tertiary">Status</span>
                  <span
                    className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
                      STATUS_BADGE[statusBadge] ?? STATUS_BADGE.disconnected
                    }`}
                  >
                    {statusBadge}
                  </span>
                </div>
                <div className="px-4 py-3 flex justify-between">
                  <span className="text-sm text-text-tertiary">Dispatch mode</span>
                  <span className="text-sm text-text-primary">
                    {DISPATCH_MODE_LABEL[channel.dispatch_mode] ?? channel.dispatch_mode}
                  </span>
                </div>
                <div className="px-4 py-3 flex justify-between">
                  <span className="text-sm text-text-tertiary">Agent</span>
                  <span className="text-sm text-text-primary">
                    {agents.find((a) => a.id === channel.agent_id)?.name ?? channel.agent_id}
                  </span>
                </div>
                <div className="px-4 py-3 flex justify-between">
                  <span className="text-sm text-text-tertiary">Created</span>
                  <span className="text-sm text-text-primary">
                    {formatDistanceToNow(new Date(channel.created_at), { addSuffix: true })}
                  </span>
                </div>
                {channel.last_started_at && (
                  <div className="px-4 py-3 flex justify-between">
                    <span className="text-sm text-text-tertiary">Last started</span>
                    <span className="text-sm text-text-primary">
                      {formatDistanceToNow(new Date(channel.last_started_at), { addSuffix: true })}
                    </span>
                  </div>
                )}
                {channel.error_message && (
                  <div className="px-4 py-3 flex justify-between gap-4">
                    <span className="text-sm text-text-tertiary shrink-0">Last error</span>
                    <span className="text-sm text-red-400 text-right break-words">
                      {channel.error_message}
                    </span>
                  </div>
                )}
              </div>

              {channel.setup
                && (channel.setup.code || channel.setup.qr) && (
                <div className="rounded-xl border border-blue-500/30 bg-blue-500/5 p-4 space-y-4">
                  <div>
                    <h4 className="text-sm font-semibold text-text-primary">
                      Awaiting setup
                    </h4>
                    {channel.setup.instructions ? (
                      <p className="text-xs text-text-tertiary mt-1 whitespace-pre-line">
                        {channel.setup.instructions}
                      </p>
                    ) : (
                      <p className="text-xs text-text-tertiary mt-1">
                        Complete the {channel.setup.qr ? "QR scan" : "code entry"} below to finish provider setup.
                      </p>
                    )}
                  </div>

                  {channel.setup.qr && (
                    <div className="flex justify-center rounded-lg bg-white p-4">
                      <QRCodeSVG
                        value={channel.setup.qr}
                        size={224}
                        level="M"
                        marginSize={0}
                      />
                    </div>
                  )}

                  {channel.setup.code && (
                    <div className="flex items-center gap-2">
                      <code className="flex-1 rounded-lg border border-border bg-surface px-3 py-3 text-center text-2xl font-mono font-semibold tracking-[0.3em] text-text-primary">
                        {channel.setup.code}
                      </code>
                      <button
                        onClick={() => navigator.clipboard.writeText(channel.setup!.code!)}
                        className="rounded-lg border border-border px-3 py-2 text-xs text-text-secondary hover:bg-surface-tertiary transition"
                      >
                        Copy
                      </button>
                    </div>
                  )}

                  {channel.setup.expires_at && (
                    <div className="text-[11px] text-text-tertiary text-center">
                      Expires {formatDistanceToNow(new Date(channel.setup.expires_at), { addSuffix: true })}
                    </div>
                  )}
                </div>
              )}

              {channel.user_address?.pairing_code && (
                <div className="rounded-xl border border-purple-500/30 bg-purple-500/5 p-4 space-y-3">
                  <div>
                    <h4 className="text-sm font-semibold text-text-primary">
                      Awaiting pairing
                    </h4>
                    <p className="text-xs text-text-tertiary mt-1">
                      Send the code below as a message to this channel from the
                      account you want to pair. The channel will flip to Connected
                      automatically.
                    </p>
                  </div>
                  <div className="flex items-center gap-2">
                    <code className="flex-1 rounded-lg border border-border bg-surface px-3 py-3 text-center text-2xl font-mono font-semibold tracking-[0.3em] text-text-primary">
                      {channel.user_address.pairing_code}
                    </code>
                    <button
                      onClick={() => navigator.clipboard.writeText(channel.user_address!.pairing_code!)}
                      className="rounded-lg border border-border px-3 py-2 text-xs text-text-secondary hover:bg-surface-tertiary transition"
                    >
                      Copy
                    </button>
                  </div>
                  <button
                    onClick={cancelPairing}
                    disabled={actionLoading}
                    className="text-xs text-text-tertiary hover:text-danger transition"
                  >
                    Cancel pairing
                  </button>
                </div>
              )}

              {channel.user_address?.address && !channel.user_address?.pairing_code && (
                <div className="rounded-xl border border-border bg-surface-secondary px-4 py-3 flex items-center justify-between">
                  <div>
                    <span className="text-xs text-text-tertiary">Paired as</span>
                    <div className="text-sm font-mono text-text-primary mt-0.5">
                      {channel.user_address.address}
                    </div>
                  </div>
                  <button
                    onClick={initiatePairing}
                    disabled={actionLoading}
                    className="text-xs text-accent hover:text-accent-hover transition"
                  >
                    Re-pair
                  </button>
                </div>
              )}

              <div className="flex items-center gap-2">
                {canStart && (
                  <button
                    onClick={start}
                    disabled={actionLoading}
                    className="inline-flex items-center gap-1.5 rounded-lg bg-green-600 px-4 py-2 text-sm font-medium text-white hover:bg-green-700 disabled:opacity-50 transition"
                  >
                    <PlayIcon className="h-4 w-4" />
                    {actionLoading ? `${startLabel}...` : startLabel}
                  </button>
                )}
                {canStop && (
                  <button
                    onClick={stop}
                    disabled={actionLoading}
                    className="inline-flex items-center gap-1.5 rounded-lg bg-yellow-600 px-4 py-2 text-sm font-medium text-white hover:bg-yellow-700 disabled:opacity-50 transition"
                  >
                    <StopIcon className="h-4 w-4" />
                    {actionLoading ? "Disabling..." : "Disable"}
                  </button>
                )}
                {!channel.user_address?.pairing_code && !channel.user_address?.address && (
                  <button
                    onClick={initiatePairing}
                    disabled={actionLoading}
                    className="inline-flex items-center gap-1.5 rounded-lg bg-purple-600 px-4 py-2 text-sm font-medium text-white hover:bg-purple-700 disabled:opacity-50 transition"
                  >
                    {actionLoading ? "Generating..." : "Pair"}
                  </button>
                )}
                <button
                  onClick={() => {
                    setDeleteAssociatedSpace(false);
                    setConfirmDeleteOpen(true);
                  }}
                  disabled={actionLoading}
                  className="inline-flex items-center gap-1.5 rounded-lg border border-border px-4 py-2 text-sm font-medium text-danger hover:bg-surface-tertiary disabled:opacity-50 transition"
                >
                  <TrashIcon className="h-4 w-4" />
                  Delete
                </button>
              </div>
            </div>
          )}

          {activeSection === "config" && (
            <div className="space-y-4">
              <SectionHeader
                title="Config"
                description="Provider-specific settings, agent binding, and dispatch mode"
                icon={Cog6ToothIcon}
              />

              <SectionPanel title="Routing">
                <div className="space-y-3">
                  <div className="space-y-1">
                    <label className="text-xs font-medium text-text-secondary">Agent</label>
                    <select
                      value={agentId}
                      onChange={(e) => {
                        setAgentId(e.target.value);
                        setDirty(true);
                      }}
                      className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary focus:border-accent focus:outline-none"
                    >
                      {agents.map((a) => (
                        <option key={a.id} value={a.id}>
                          {a.name}
                        </option>
                      ))}
                    </select>
                  </div>

                  <div className="space-y-1">
                    <label className="text-xs font-medium text-text-secondary">
                      When a message arrives
                    </label>
                    <select
                      value={dispatchMode}
                      onChange={(e) => {
                        setDispatchMode(e.target.value as "message" | "signal");
                        setDirty(true);
                      }}
                      className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary focus:border-accent focus:outline-none"
                    >
                      <option value="message">Treat as a message from you — requires pairing</option>
                      <option value="signal">Hand off to a waiting agent — e.g. 2FA codes or confirmation links</option>
                    </select>
                  </div>
                </div>
              </SectionPanel>

              {channel.webhook_url && (
                <SectionPanel title="Webhook URL">
                  <p className="text-sm text-text-secondary mb-2">
                    Paste this into the provider&apos;s webhook configuration.
                  </p>
                  <WebhookUrlField url={channel.webhook_url} />
                </SectionPanel>
              )}

              {manifest && manifest.config_fields.length > 0 && (
                <SectionPanel title="Provider Config">
                  <div className="space-y-3">
                    {manifest.config_fields.map((f) => {
                      const liveGrant = grants
                        .filter((g) => !deletedGrantIds.has(g.id))
                        .find((g) => g.query === f.name);
                      const pending = pendingCreds.find((p) => p.query === f.name);
                      return (
                        <div key={f.name} className="space-y-1">
                          <div className="flex items-center gap-2">
                            <label className="text-xs font-medium text-text-secondary">
                              {f.description || f.name}
                            </label>
                            {f.is_required && (
                              <span className="text-[10px] text-red-400">required</span>
                            )}
                          </div>
                          {f.is_secret ? (
                            liveGrant ? (
                              <div className="flex items-center gap-2">
                                <KeyIcon className="h-4 w-4 text-text-tertiary shrink-0" />
                                <span className="text-sm text-text-primary truncate">
                                  Configured
                                  <span className="text-text-tertiary"> ({connections.get(liveGrant.connection_id)?.name ?? "vault"})</span>
                                </span>
                                <button
                                  type="button"
                                  onClick={() => {
                                    setCredDialogExisting({
                                      connection_id: liveGrant.connection_id,
                                      vault_item_id: liveGrant.vault_item_id,
                                    });
                                    setCredDialogEnvVar(f.name);
                                  }}
                                  className="text-xs text-accent hover:underline"
                                >
                                  Change
                                </button>
                                <button
                                  type="button"
                                  onClick={() => {
                                    setDeletedGrantIds((prev) => new Set(prev).add(liveGrant.id));
                                    setDirty(true);
                                  }}
                                  className="text-xs text-danger hover:underline"
                                >
                                  Remove
                                </button>
                              </div>
                            ) : pending ? (
                              <div className="flex items-center gap-2">
                                <KeyIcon className="h-4 w-4 text-text-tertiary shrink-0" />
                                <span className="text-sm text-text-primary truncate">
                                  {pending.item_name}
                                  <span className="text-text-tertiary"> ({pending.connection_name})</span>
                                </span>
                                <span className="rounded-full bg-yellow-500/15 px-2 py-0.5 text-[10px] font-medium text-yellow-500">
                                  Pending save
                                </span>
                                <button
                                  type="button"
                                  onClick={() => {
                                    setPendingCreds((prev) => prev.filter((p) => p.query !== f.name));
                                    setDirty(true);
                                  }}
                                  className="text-xs text-danger hover:underline"
                                >
                                  Discard
                                </button>
                              </div>
                            ) : f.default_from ? (
                              <div className="flex items-center gap-2">
                                <KeyIcon className="h-4 w-4 text-text-tertiary shrink-0" />
                                <span className="text-sm text-text-primary truncate">
                                  Using server default
                                  <span className="text-text-tertiary"> ({f.default_from.section}.{f.default_from.field})</span>
                                </span>
                                <button
                                  type="button"
                                  onClick={() => {
                                    setCredDialogExisting(undefined);
                                    setCredDialogEnvVar(f.name);
                                  }}
                                  className="text-xs text-accent hover:underline"
                                >
                                  Override
                                </button>
                              </div>
                            ) : (
                              <button
                                type="button"
                                onClick={() => {
                                  setCredDialogExisting(undefined);
                                  setCredDialogEnvVar(f.name);
                                }}
                                className="text-sm transition cursor-pointer"
                              >
                                <span className="text-text-tertiary">Not set — </span>
                                <span className="text-accent hover:underline">Configure</span>
                              </button>
                            )
                          ) : (
                            <input
                              type="text"
                              value={config[f.name] ?? ""}
                              onChange={(e) => {
                                setConfig((prev) => ({ ...prev, [f.name]: e.target.value }));
                                setDirty(true);
                              }}
                              placeholder={
                                f.default_resolved
                                  ? `default: ${f.default_resolved}`
                                  : f.is_required
                                    ? "Required"
                                    : "Optional"
                              }
                              className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none font-mono"
                            />
                          )}
                        </div>
                      );
                    })}
                  </div>
                </SectionPanel>
              )}

              <SectionPanel>
                <h3 className="text-sm font-medium text-text-primary mb-3">Retry policy</h3>
                <p className="text-xs text-text-tertiary mb-4">
                  Override the channel-restart backoff for this channel. Leave blank to inherit the
                  global default. Set max retries to <code>0</code> to disable auto-retry.
                </p>
                <div className="grid grid-cols-2 gap-3">
                  <RetryNumberInput
                    label="Max retries"
                    value={retry?.max_retries}
                    placeholder="Forever"
                    onChange={(v) => {
                      const next = retry ?? { ...CHANNEL_RETRY_DEFAULTS };
                      setRetry({ ...next, max_retries: v ?? CHANNEL_RETRY_DEFAULTS.max_retries });
                      setDirty(true);
                    }}
                    forever={retry?.max_retries === RETRY_FOREVER}
                  />
                  <RetryNumberInput
                    label="Initial backoff (ms)"
                    value={retry?.initial_backoff_ms}
                    placeholder={`${CHANNEL_RETRY_DEFAULTS.initial_backoff_ms}`}
                    onChange={(v) => {
                      const next = retry ?? { ...CHANNEL_RETRY_DEFAULTS };
                      setRetry({
                        ...next,
                        initial_backoff_ms: v ?? CHANNEL_RETRY_DEFAULTS.initial_backoff_ms,
                      });
                      setDirty(true);
                    }}
                  />
                  <RetryNumberInput
                    label="Backoff multiplier"
                    value={retry?.backoff_multiplier}
                    placeholder={`${CHANNEL_RETRY_DEFAULTS.backoff_multiplier}`}
                    step={0.1}
                    onChange={(v) => {
                      const next = retry ?? { ...CHANNEL_RETRY_DEFAULTS };
                      setRetry({
                        ...next,
                        backoff_multiplier: v ?? CHANNEL_RETRY_DEFAULTS.backoff_multiplier,
                      });
                      setDirty(true);
                    }}
                  />
                  <RetryNumberInput
                    label="Max backoff (ms)"
                    value={retry?.max_backoff_ms}
                    placeholder={`${CHANNEL_RETRY_DEFAULTS.max_backoff_ms}`}
                    onChange={(v) => {
                      const next = retry ?? { ...CHANNEL_RETRY_DEFAULTS };
                      setRetry({
                        ...next,
                        max_backoff_ms: v ?? CHANNEL_RETRY_DEFAULTS.max_backoff_ms,
                      });
                      setDirty(true);
                    }}
                  />
                </div>
                {retry !== null && (
                  <button
                    type="button"
                    onClick={() => {
                      setRetry(null);
                      setDirty(true);
                    }}
                    className="mt-3 text-xs text-text-secondary hover:text-text-primary transition"
                  >
                    Reset to inherit global default
                  </button>
                )}
              </SectionPanel>
            </div>
          )}

          {credDialogEnvVar !== null && channel && (
            <AddCredentialForm
              connections={connections}
              principalKind="channel"
              principalId={channel.id}
              existingGrants={grants}
              targetEnvVar={credDialogEnvVar}
              initialSelection={credDialogExisting}
              onClose={() => {
                setCredDialogEnvVar(null);
                setCredDialogExisting(undefined);
              }}
              // For "Change", stage the removal of the existing grant alongside
              // the new pending credential so Save commits the swap atomically.
              deferred={(pending) => {
                if (credDialogExisting) {
                  const replacing = grants.find(
                    (g) =>
                      g.connection_id === credDialogExisting.connection_id
                      && g.vault_item_id === credDialogExisting.vault_item_id,
                  );
                  if (replacing) {
                    setDeletedGrantIds((prev) => new Set(prev).add(replacing.id));
                  }
                }
                setPendingCreds((prev) => [
                  ...prev.filter((p) => p.query !== pending.query),
                  pending,
                ]);
                setDirty(true);
                setCredDialogEnvVar(null);
                setCredDialogExisting(undefined);
              }}
              onCreated={() => {
                setCredDialogEnvVar(null);
                setCredDialogExisting(undefined);
              }}
            />
          )}

          {activeSection === "config" && (
            <div className="pt-4 border-t border-border flex items-center justify-end gap-2">
              <button
                onClick={() => {
                  reload();
                }}
                disabled={!dirty}
                className="rounded-lg border border-border px-4 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary disabled:opacity-50 transition"
              >
                Discard
              </button>
              <button
                onClick={save}
                disabled={!dirty || saving}
                className="rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover disabled:opacity-50 transition"
              >
                {saving ? "Saving..." : "Save"}
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

export default function ChannelsPage() {
  return (
    <Suspense>
      <ChannelDetailPage />
    </Suspense>
  );
}
