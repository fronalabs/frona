"use client";

import { useState, useEffect } from "react";
import { useSession } from "@/lib/session-context";
import { api } from "@/lib/api-client";
import type { MessageResponse } from "@/lib/types";

function QuestionMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  const { resolveToolMessage } = useSession();
  const [loading, setLoading] = useState(false);
  const [answered, setAnswered] = useState<string | null>(null);

  if (!message.tool || message.tool.type !== "Question") return null;

  const resolved = message.tool.data.status === "resolved";
  const selectedAnswer = answered ?? message.tool.data.response;

  const handleAnswer = async (answer: string) => {
    setLoading(true);
    setAnswered(answer);
    try {
      await resolveToolMessage(message.id, answer);
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex justify-start">
      <div className="flex items-start gap-2.5 max-w-[85%]">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-surface-tertiary text-text-secondary">
          {agentName.charAt(0).toUpperCase()}
        </div>
        <div className="min-w-0 pt-0.5">
          <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
            {agentName}
          </p>
          <p className="text-sm text-text-primary mb-2">
            {message.tool.data.question}
          </p>
          <div className="flex flex-col gap-2">
            {message.tool.data.options.map((option) => {
              const isSelected = selectedAnswer === option;
              return (
                <button
                  key={option}
                  onClick={() => handleAnswer(option)}
                  disabled={loading || resolved || answered !== null}
                  className={`rounded-lg border px-3 py-1.5 text-left text-sm font-medium transition ${
                    isSelected
                      ? "border-accent bg-accent/10 text-accent"
                      : resolved || answered !== null
                        ? "border-border text-text-tertiary opacity-50"
                        : "border-border text-text-secondary hover:border-accent hover:text-accent"
                  }`}
                >
                  {option}
                </button>
              );
            })}
          </div>
        </div>
      </div>
    </div>
  );
}

function HumanInTheLoopMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  const { resolveToolMessage } = useSession();
  const [loading, setLoading] = useState(false);

  if (!message.tool || message.tool.type !== "HumanInTheLoop") return null;

  const resolved = message.tool.data.status === "resolved";

  const handleResume = async () => {
    setLoading(true);
    try {
      await resolveToolMessage(message.id, "resumed");
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex justify-start">
      <div className="flex items-start gap-2.5 max-w-[85%]">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-surface-tertiary text-text-secondary">
          {agentName.charAt(0).toUpperCase()}
        </div>
        <div className="min-w-0 pt-0.5">
          <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
            {agentName}
          </p>
          <p className="text-sm text-text-primary mb-2">
            {message.tool.data.reason}
          </p>
          <div className="flex flex-wrap gap-2">
            {message.tool.data.debugger_url && (
              <a
                href={message.tool.data.debugger_url}
                target="_blank"
                rel="noopener noreferrer"
                className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium text-text-secondary hover:border-accent hover:text-accent transition"
              >
                Open Browser Debugger
              </a>
            )}
            <button
              onClick={handleResume}
              disabled={loading || resolved}
              className={`rounded-lg border px-3 py-1.5 text-sm font-medium transition ${
                resolved
                  ? "border-accent bg-accent/10 text-accent"
                  : "border-border text-text-secondary hover:border-accent hover:text-accent"
              }`}
            >
              {loading ? "Resuming..." : resolved ? "Resumed" : "Resume Agent"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

function TaskCompletionMessage({ message }: { message: MessageResponse }) {
  if (!message.tool || message.tool.type !== "TaskCompletion") return null;

  const { status } = message.tool.data;
  const isError = status === "failed";

  return (
    <div
      className={`flex items-start gap-3 rounded-lg border px-4 py-3 text-sm ${
        isError
          ? "border-red-200 bg-red-50 text-red-800 dark:border-red-800 dark:bg-red-950 dark:text-red-200"
          : "border-green-200 bg-green-50 text-green-800 dark:border-green-800 dark:bg-green-950 dark:text-green-200"
      }`}
    >
      <span className="flex-1">{message.content}</span>
    </div>
  );
}

interface VaultItem {
  id: string;
  name: string;
  username?: string;
}

interface VaultConnection {
  id: string;
  name: string;
  provider: string;
  enabled: boolean;
}

type GrantDuration =
  | "once"
  | { hours: number }
  | { days: number }
  | "permanent";

function VaultApprovalMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  const { activeChatId } = useSession();
  const [loading, setLoading] = useState(false);
  const [connections, setConnections] = useState<VaultConnection[]>([]);
  const [selectedConnection, setSelectedConnection] = useState<string>("");
  const [items, setItems] = useState<VaultItem[]>([]);
  const [selectedItem, setSelectedItem] = useState<string>("");
  const [duration, setDuration] = useState<GrantDuration>("once");
  const [searchQuery, setSearchQuery] = useState("");
  const [searching, setSearching] = useState(false);

  if (!message.tool || message.tool.type !== "VaultApproval") return null;

  const { status: toolStatus, query, reason, env_var_prefix } = message.tool.data;
  const resolved = toolStatus === "resolved";
  const denied = toolStatus === "denied";

  useEffect(() => {
    api.get<VaultConnection[]>("/api/vaults").then((conns) => {
      setConnections(conns.filter((c) => c.enabled));
      if (conns.length > 0) {
        setSelectedConnection(conns[0].id);
      }
    });
    setSearchQuery(query);
  }, [query]);

  useEffect(() => {
    if (!selectedConnection || !searchQuery) return;
    setSearching(true);
    api
      .get<VaultItem[]>(
        `/api/vaults/${selectedConnection}/items?q=${encodeURIComponent(searchQuery)}`,
      )
      .then((results) => {
        setItems(results);
        if (results.length > 0) {
          setSelectedItem(results[0].id);
        }
      })
      .finally(() => setSearching(false));
  }, [selectedConnection, searchQuery]);

  const handleApprove = async () => {
    if (!activeChatId || !selectedItem) return;
    setLoading(true);
    try {
      await api.post("/api/vaults/approve", {
        chat_id: activeChatId,
        connection_id: selectedConnection,
        vault_item_id: selectedItem,
        grant_duration: duration,
        env_var_prefix,
      });
    } finally {
      setLoading(false);
    }
  };

  const handleDeny = async () => {
    if (!activeChatId) return;
    setLoading(true);
    try {
      await api.post("/api/vaults/deny", { chat_id: activeChatId });
    } finally {
      setLoading(false);
    }
  };

  if (resolved || denied) {
    const colorClasses = denied
      ? "border-red-200 bg-red-50 text-red-800 dark:border-red-800 dark:bg-red-950 dark:text-red-200"
      : "border-green-200 bg-green-50 text-green-800 dark:border-green-800 dark:bg-green-950 dark:text-green-200";
    const label = denied ? "Credential request denied" : "Credential request approved";

    return (
      <div className="flex justify-start">
        <div className="flex items-start gap-2.5 max-w-[85%]">
          <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-surface-tertiary text-text-secondary">
            {agentName.charAt(0).toUpperCase()}
          </div>
          <div className="min-w-0 pt-0.5">
            <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
              {agentName}
            </p>
            <div className={`rounded-lg border px-3 py-2 text-sm ${colorClasses}`}>
              {label}
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex justify-start">
      <div className="flex items-start gap-2.5 max-w-[85%]">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-surface-tertiary text-text-secondary">
          {agentName.charAt(0).toUpperCase()}
        </div>
        <div className="min-w-0 pt-0.5 w-full">
          <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
            {agentName}
          </p>
          <div className="rounded-lg border border-border p-3 space-y-3">
            <p className="text-sm font-medium text-text-primary">
              Credential Request
            </p>
            <p className="text-sm text-text-secondary">{reason}</p>

            <div className="space-y-2">
              <label className="block text-xs font-medium text-text-tertiary">
                Vault
              </label>
              <select
                value={selectedConnection}
                onChange={(e) => setSelectedConnection(e.target.value)}
                className="w-full rounded-md border border-border bg-surface-primary px-2 py-1.5 text-sm text-text-primary"
              >
                {connections.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
            </div>

            <div className="space-y-2">
              <label className="block text-xs font-medium text-text-tertiary">
                Search
              </label>
              <input
                type="text"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                className="w-full rounded-md border border-border bg-surface-primary px-2 py-1.5 text-sm text-text-primary"
                placeholder="Search vault items..."
              />
            </div>

            {searching ? (
              <p className="text-xs text-text-tertiary">Searching...</p>
            ) : items.length > 0 ? (
              <div className="space-y-1">
                {items.map((item) => (
                  <button
                    key={item.id}
                    onClick={() => setSelectedItem(item.id)}
                    className={`w-full rounded-md border px-2 py-1.5 text-left text-sm transition ${
                      selectedItem === item.id
                        ? "border-accent bg-accent/10 text-accent"
                        : "border-border text-text-secondary hover:border-accent"
                    }`}
                  >
                    <span className="font-medium">{item.name}</span>
                    {item.username && (
                      <span className="ml-2 text-text-tertiary">
                        ({item.username})
                      </span>
                    )}
                  </button>
                ))}
              </div>
            ) : (
              <p className="text-xs text-text-tertiary">No items found</p>
            )}

            <div className="space-y-2">
              <label className="block text-xs font-medium text-text-tertiary">
                Duration
              </label>
              <select
                value={
                  typeof duration === "string"
                    ? duration
                    : "hours" in duration
                      ? "hours"
                      : "days"
                }
                onChange={(e) => {
                  const v = e.target.value;
                  if (v === "once") setDuration("once");
                  else if (v === "permanent") setDuration("permanent");
                  else if (v === "hours") setDuration({ hours: 24 });
                  else if (v === "days") setDuration({ days: 7 });
                }}
                className="w-full rounded-md border border-border bg-surface-primary px-2 py-1.5 text-sm text-text-primary"
              >
                <option value="once">Allow once</option>
                <option value="hours">Allow for 24 hours</option>
                <option value="days">Allow for 7 days</option>
                <option value="permanent">Allow permanently</option>
              </select>
            </div>

            <div className="flex gap-2">
              <button
                onClick={handleApprove}
                disabled={loading || !selectedItem}
                className="rounded-lg border border-accent bg-accent/10 px-3 py-1.5 text-sm font-medium text-accent hover:bg-accent/20 transition disabled:opacity-50"
              >
                {loading ? "Approving..." : "Approve"}
              </button>
              <button
                onClick={handleDeny}
                disabled={loading}
                className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium text-text-secondary hover:border-red-400 hover:text-red-500 transition"
              >
                Deny
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

export function ToolMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  if (!message.tool) return null;

  switch (message.tool.type) {
    case "Question":
      return <QuestionMessage message={message} agentName={agentName} />;
    case "HumanInTheLoop":
      return <HumanInTheLoopMessage message={message} agentName={agentName} />;
    case "TaskCompletion":
      return <TaskCompletionMessage message={message} />;
    case "VaultApproval":
      return <VaultApprovalMessage message={message} agentName={agentName} />;
    default:
      return null;
  }
}
