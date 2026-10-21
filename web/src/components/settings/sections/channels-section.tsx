"use client";

import { useState, useEffect, useCallback } from "react";
import { SectionHeader } from "../field";
import {
  ChatBubbleLeftRightIcon,
  PlayIcon,
  StopIcon,
  TrashIcon,
  PlusIcon,
  XMarkIcon,
} from "@heroicons/react/24/outline";
import { api } from "@/lib/api-client";
import { ComboboxInput } from "@/components/settings/combobox";
import { HelpTip } from "@/components/settings/field";
import { ManifestInfo, markdownToPlainText, type ExternalLink } from "@/components/channels/manifest-info";
import { formatDistanceToNow } from "date-fns";
import { useRouter } from "next/navigation";
import type { Agent, SpaceResponse } from "@/lib/types";

interface UserAddress {
  address: string | null;
  pairing_code: string | null;
  pairing_initiated_at: string | null;
  paired_at: string | null;
}

interface SetupConfig {
  qr: string | null;
  code: string | null;
  instructions: string | null;
  expires_at: string | null;
}

interface Channel {
  id: string;
  user_id: string;
  space_id: string;
  provider: string;
  agent_id: string;
  config: Record<string, string>;
  dispatch_mode: "message" | "signal";
  // Connection status only - supervisor-owned. Setup/pairing are overlays below.
  status: "disconnected" | "connecting" | "connected" | "reconnecting" | "failed";
  // Operator intent, independent of runtime status.
  enabled: boolean;
  error_message: string | null;
  last_started_at: string | null;
  user_address: UserAddress | null;
  setup: SetupConfig | null;
  created_at: string;
  updated_at: string;
}

interface ChannelConfigField {
  name: string;
  description: string | null;
  is_required: boolean;
  is_secret: boolean;
  format: string | null;
  default_resolved: string | null;
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

// Keyed by the *derived* badge label (connection status + overlays), not the
// raw status - see `channelBadge` below.
const STATUS_BADGE: Record<string, string> = {
  disconnected: "bg-surface-tertiary text-text-secondary",
  connecting: "bg-blue-400/15 text-blue-400",
  connected: "bg-green-500/15 text-green-500",
  reconnecting: "bg-amber-500/15 text-amber-400",
  failed: "bg-red-500/15 text-red-500",
  pairing: "bg-purple-500/15 text-purple-400",
  linking: "bg-yellow-500/15 text-yellow-500",
};

/// Derives the display badge from the three independent axes: a pending QR
/// (setup overlay) or pairing code takes visual priority over the connection
/// status, which is otherwise shown as-is.
function channelBadge(c: Channel): { label: string; key: string } {
  if (c.setup) return { label: "linking", key: "linking" };
  if (c.user_address?.pairing_code) return { label: "pairing", key: "pairing" };
  return { label: c.status, key: c.status };
}

// Green is reserved for the `connected` status badge - keep it out of the
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

export function ChannelsSection() {
  const [channels, setChannels] = useState<Channel[]>([]);
  const [manifests, setManifests] = useState<ChannelManifest[]>([]);
  const [spaces, setSpaces] = useState<SpaceResponse[]>([]);
  const [loading, setLoading] = useState(true);
  const [actionLoading, setActionLoading] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [installManifest, setInstallManifest] = useState<ChannelManifest | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<Channel | null>(null);
  const [deleteAssociatedSpace, setDeleteAssociatedSpace] = useState(false);
  const router = useRouter();

  const reload = useCallback(async () => {
    try {
      const [chans, mans, spcs] = await Promise.all([
        api.get<Channel[]>("/api/channels"),
        api.get<ChannelManifest[]>("/api/channels/manifests"),
        api.get<SpaceResponse[]>("/api/spaces"),
      ]);
      setChannels(chans);
      setManifests(mans);
      setSpaces(spcs);
    } catch {
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  const start = async (id: string) => {
    setActionLoading(id);
    setError(null);
    try {
      await api.post(`/api/channels/${id}/start`, {});
      await reload();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Start failed");
    } finally {
      setActionLoading(null);
    }
  };

  const stop = async (id: string) => {
    setActionLoading(id);
    try {
      await api.post(`/api/channels/${id}/stop`, {});
      await reload();
    } catch {
    } finally {
      setActionLoading(null);
    }
  };

  const remove = async (channel: Channel) => {
    setConfirmDelete(null);
    setActionLoading(channel.id);
    setError(null);
    try {
      const query = deleteAssociatedSpace ? "?delete_space=true" : "";
      await api.delete(`/api/channels/${channel.id}${query}`);
      setDeleteAssociatedSpace(false);
      await reload();
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Delete failed");
      await reload();
    } finally {
      setActionLoading(null);
    }
  };

  return (
    <div className="space-y-4">
      <SectionHeader
        title="Channels"
        description="Connect external messaging providers (Telegram, SMS, ...) so agents can send and receive messages."
        icon={ChatBubbleLeftRightIcon}
      />

      {confirmDelete && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div
            className="absolute inset-0 bg-black/50"
            onClick={() => {
              setConfirmDelete(null);
              setDeleteAssociatedSpace(false);
            }}
          />
          <div className="relative rounded-xl border border-border bg-surface-secondary p-4 space-y-4 max-w-lg w-full mx-4 shadow-xl">
            <div className="mb-5 pb-3 border-b border-border flex items-end justify-between gap-3">
              <div>
                <h3 className="text-lg font-semibold text-text-primary">
                  {confirmDelete.provider}
                </h3>
                <span className="rounded-full bg-surface-tertiary px-2.5 py-0.5 text-[11px] font-medium text-text-secondary uppercase tracking-wide">
                  delete
                </span>
              </div>
              <TrashIcon className="h-10 w-10 text-danger shrink-0" />
            </div>
            <p className="text-sm text-text-secondary">
              This will stop the channel and remove it. The agent in this space will no
              longer receive or send messages through this provider.
            </p>
            {(() => {
              const space = spaces.find((s) => s.id === confirmDelete.space_id);
              const chatCount = space?.chat_count ?? 0;
              return (
                <label className="flex items-start gap-2 rounded-lg border border-border bg-surface p-3 text-sm text-text-secondary">
                  <input
                    type="checkbox"
                    checked={deleteAssociatedSpace}
                    onChange={(e) => setDeleteAssociatedSpace(e.target.checked)}
                    className="mt-0.5"
                  />
                  <span>
                    Also delete the associated space{space ? ` "${space.name}"` : ""}
                    {chatCount > 0
                      ? ` and its ${chatCount} ${chatCount === 1 ? "chat" : "chats"}, including all messages`
                      : ""}.
                  </span>
                </label>
              );
            })()}
            <div className="flex gap-2">
              <button
                onClick={() => remove(confirmDelete)}
                className="w-28 inline-flex items-center justify-center gap-1.5 rounded-lg border border-border py-2 text-sm font-medium text-danger hover:bg-surface-tertiary transition"
              >
                <TrashIcon className="h-4 w-4" />
                Delete
              </button>
              <button
                onClick={() => {
                  setConfirmDelete(null);
                  setDeleteAssociatedSpace(false);
                }}
                className="w-28 inline-flex items-center justify-center gap-1.5 rounded-lg border border-border py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary transition"
              >
                Cancel
              </button>
            </div>
          </div>
        </div>
      )}

      {installManifest && (
        <CreateChannelDialog
          manifest={installManifest}
          channels={channels}
          onClose={() => setInstallManifest(null)}
          onCreated={(channel) => {
            setInstallManifest(null);
            reload().then(() => router.push(`/channels?id=${channel.id}&section=config`));
          }}
        />
      )}

      {error && (
        <div className="rounded-lg bg-error-bg p-3 text-sm text-error-text">{error}</div>
      )}

      <div>
        <div className="flex items-center justify-between mb-2 min-h-[36px]">
          <h4 className="text-base font-medium text-text-secondary">Providers</h4>
        </div>
        {manifests.length === 0 && !loading ? (
          <p className="text-sm text-text-tertiary text-center py-8">
            No channel providers registered.
          </p>
        ) : (
          <div className="rounded-xl border border-border bg-surface-secondary divide-y divide-border overflow-hidden">
            {manifests.map((m) => (
              <div key={m.id} className="px-4 py-3 flex items-start gap-3">
                <ChatBubbleLeftRightIcon className="h-8 w-8 rounded-lg shrink-0 mt-0.5 text-text-tertiary" />
                <div className="flex-1 min-w-0">
                  <div className="text-sm font-medium text-text-primary">{m.display_name}</div>
                  <div className="text-xs text-text-tertiary line-clamp-2">{markdownToPlainText(m.description)}</div>
                </div>
                <button
                  onClick={() => setInstallManifest(m)}
                  className="shrink-0 inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-surface shadow-sm hover:bg-accent-hover transition"
                >
                  <PlusIcon className="h-3.5 w-3.5" />
                  Add
                </button>
              </div>
            ))}
          </div>
        )}
      </div>

      <div>
        {!loading && channels.length > 0 && (
          <div className="flex items-center justify-between mb-2 min-h-[36px]">
            <h4 className="text-base font-medium text-text-secondary">Configured</h4>
          </div>
        )}
        {loading ? (
          <div className="flex items-center justify-center py-12">
            <div className="h-5 w-5 animate-spin rounded-full border-2 border-accent border-t-transparent" />
          </div>
        ) : channels.length === 0 ? (
          <p className="text-sm text-text-tertiary text-center py-8">
            No channels configured. Add one from the providers above.
          </p>
        ) : (
          <div className="rounded-xl border border-border bg-surface-secondary divide-y divide-border overflow-hidden">
            {channels.map((c) => {
              const isLoading = actionLoading === c.id;
              const badge = channelBadge(c);
              // Buttons follow intent, not runtime status. Start enables (or retries a
              // terminally-failed channel); Stop disables. Both hit /start, /stop.
              const showStart = !c.enabled || c.status === "failed";
              const showStop = c.enabled;
              const startTitle = c.enabled ? "Retry" : "Enable";
              const manifest = manifests.find((m) => m.id === c.provider);
              const space = spaces.find((s) => s.id === c.space_id);
              const title = space?.name ?? manifest?.display_name ?? c.provider;
              return (
                <div
                  key={c.id}
                  onClick={(e) => {
                    if (!(e.target as HTMLElement).closest("button")) {
                      router.push(`/channels?id=${c.id}`);
                    }
                  }}
                  className="px-4 py-3 flex items-center gap-3 transition hover:bg-surface-tertiary cursor-pointer"
                >
                  <ChatBubbleLeftRightIcon className="h-8 w-8 shrink-0 text-text-tertiary" />
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2">
                      <span className="text-sm font-medium text-text-primary truncate">
                        {title}
                      </span>
                      <span
                        className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${providerBadgeClass(c.provider, manifests)}`}
                      >
                        {manifest?.display_name ?? c.provider}
                      </span>
                      <span
                        className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${
                          STATUS_BADGE[badge.key] ?? "bg-surface-tertiary text-text-secondary"
                        }`}
                      >
                        {badge.label}
                      </span>
                      <span className="rounded-full bg-surface-tertiary px-2 py-0.5 text-[11px] text-text-tertiary">
                        {c.dispatch_mode}
                      </span>
                    </div>
                    {c.error_message && (
                      <div className="text-xs text-red-400 line-clamp-2 mt-0.5">
                        {c.error_message}
                      </div>
                    )}
                  </div>
                  <div className="flex items-center gap-1 shrink-0">
                    {showStart && (
                      <button
                        onClick={() => start(c.id)}
                        disabled={isLoading}
                        title={startTitle}
                        className="rounded-lg p-1.5 text-green-500 hover:bg-green-500/10 disabled:opacity-50 transition"
                      >
                        <PlayIcon className="h-5 w-5" />
                      </button>
                    )}
                    {showStop && (
                      <button
                        onClick={() => stop(c.id)}
                        disabled={isLoading}
                        title="Disable"
                        className="rounded-lg p-1.5 text-yellow-500 hover:bg-yellow-500/10 disabled:opacity-50 transition"
                      >
                        <StopIcon className="h-5 w-5" />
                      </button>
                    )}
                    <button
                      onClick={() => {
                        setDeleteAssociatedSpace(false);
                        setConfirmDelete(c);
                      }}
                      disabled={isLoading}
                      title="Delete"
                      className="rounded-lg p-1.5 text-text-tertiary hover:text-danger hover:bg-danger/10 disabled:opacity-50 transition"
                    >
                      <TrashIcon className="h-5 w-5" />
                    </button>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

function CreateChannelDialog({
  manifest,
  channels,
  onClose,
  onCreated,
}: {
  manifest: ChannelManifest;
  channels: Channel[];
  onClose: () => void;
  onCreated: (channel: Channel) => void;
}) {
  const [agents, setAgents] = useState<Agent[]>([]);
  const [spaces, setSpaces] = useState<SpaceResponse[]>([]);
  const [agentId, setAgentId] = useState<string>("");
  const [spaceName, setSpaceName] = useState<string>(manifest.display_name);
  const [dispatchMode, setDispatchMode] = useState<"message" | "signal">("message");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // A space can host at most one channel (`idx_channel_space_unique`),
  // so spaces already bound to a channel are not selectable here.
  const usedSpaceIds = new Set(channels.map((c) => c.space_id));
  const availableSpaces = spaces.filter((s) => !usedSpaceIds.has(s.id));
  const spaceItems = availableSpaces.map((s) => ({ value: s.name, label: s.name }));

  useEffect(() => {
    Promise.all([
      api.get<Agent[]>("/api/agents"),
      api.get<SpaceResponse[]>("/api/spaces"),
    ])
      .then(([a, s]) => {
        setAgents(a);
        setSpaces(s);
        if (a[0]) setAgentId(a[0].id);
      })
      .catch(() => {});
  }, []);

  const submit = async () => {
    setError(null);
    if (!agentId) {
      setError("Agent is required");
      return;
    }
    const trimmedName = spaceName.trim();
    if (!trimmedName) {
      setError("Name the space");
      return;
    }
    setSubmitting(true);
    try {
      const existing = availableSpaces.find((s) => s.name === trimmedName);
      const resolvedSpaceId =
        existing?.id ??
        (await api.post<SpaceResponse>("/api/spaces", { name: trimmedName })).id;
      const channel = await api.post<Channel>("/api/channels", {
        space_id: resolvedSpaceId,
        provider: manifest.id,
        agent_id: agentId,
        dispatch_mode: dispatchMode,
        config: {},
        credentials: [],
      });
      onCreated(channel);
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : "Create failed");
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/50" onClick={onClose} />
      <div className="relative rounded-xl border border-border bg-surface-secondary p-5 space-y-4 max-w-lg w-full mx-4 shadow-xl max-h-[90vh] overflow-y-auto">
        <div className="flex items-start gap-3">
          <ChatBubbleLeftRightIcon className="h-10 w-10 text-text-tertiary shrink-0" />
          <div className="flex-1 min-w-0">
            <h3 className="text-lg font-semibold text-text-primary">{manifest.display_name}</h3>
          </div>
        </div>

        <ManifestInfo manifest={manifest} />

        <div className="space-y-3">
          <ComboboxInput
            label={
              <>
                Space
                <HelpTip label="Help for Space" content="Spaces group related chats. Messages from this channel land here as new chats, and the assigned agent replies in them." />
                {!availableSpaces.some((s) => s.name === spaceName.trim()) && (
                  <span className="rounded-full bg-green-500/15 px-2 py-0.5 text-[10px] font-medium text-green-500">
                    New
                  </span>
                )}
              </>
            }
            value={spaceName}
            items={spaceItems}
            onChange={setSpaceName}
            placeholder="Type a name to create one, or pick an existing space"
            allowFreeText
          />

          <div className="space-y-1">
            <label className="flex items-center gap-2 text-xs font-medium text-text-secondary">
              Agent
              <HelpTip label="Help for Agent" content="The agent assigned to this channel. It reads every incoming message and, when configured to reply, sends the response back." />
            </label>
            <select
              value={agentId}
              onChange={(e) => setAgentId(e.target.value)}
              className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary focus:border-accent focus:outline-none"
            >
              {agents.length === 0 && <option value="">No agents available</option>}
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name}
                </option>
              ))}
            </select>
          </div>

          <div className="space-y-1">
            <label className="flex items-center gap-2 text-xs font-medium text-text-secondary">
              When a message arrives
              <HelpTip label="Help for When a message arrives" content="Treat as a message from you: the agent runs as if you wrote the message yourself. Only works once the channel is paired to your account. Hand off to a waiting agent: the message goes to whichever task is currently waiting for input, such as a 2FA code or a confirmation link." />
            </label>
            <select
              value={dispatchMode}
              onChange={(e) => setDispatchMode(e.target.value as "message" | "signal")}
              className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary focus:border-accent focus:outline-none"
            >
              <option value="message">Treat as a message from you - requires pairing</option>
              <option value="signal">Hand off to a waiting agent - e.g. 2FA codes or confirmation links</option>
            </select>
          </div>

        </div>

        {error && (
          <div className="rounded-lg bg-error-bg p-3 text-sm text-error-text">{error}</div>
        )}

        <div className="flex gap-2 pt-2">
          <button
            onClick={submit}
            disabled={submitting}
            className="w-28 inline-flex items-center justify-center gap-1.5 rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover disabled:opacity-50 transition"
          >
            <PlusIcon className="h-4 w-4" />
            {submitting ? "Creating..." : "Create"}
          </button>
          <button
            onClick={onClose}
            className="w-28 inline-flex items-center justify-center gap-1.5 rounded-lg border border-border px-4 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary transition"
          >
            <XMarkIcon className="h-4 w-4" />
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}
