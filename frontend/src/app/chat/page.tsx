"use client";

import { useState, useEffect, useRef } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { PaperAirplaneIcon } from "@heroicons/react/24/solid";
import { EllipsisVerticalIcon } from "@heroicons/react/24/outline";
import { api, API_URL } from "@/lib/api-client";
import { useSession } from "@/lib/session-context";
import { useNavigation } from "@/lib/navigation-context";
import type { ChatResponse, AppResponse } from "@/lib/types";

function SpaceView({ spaceId }: { spaceId: string }) {
  const { spaces, refresh } = useNavigation();
  const { activeChatId, setPendingMessage } = useSession();
  const router = useRouter();
  const [newMessage, setNewMessage] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const space = spaces.find((s) => s.id === spaceId);

  const handleNewChat = async (e: React.FormEvent) => {
    e.preventDefault();
    const content = newMessage.trim();
    if (!content || submitting) return;
    setSubmitting(true);
    try {
      const chat = await api.post<ChatResponse>("/api/chats", {
        space_id: spaceId,
        agent_id: "system",
      });
      setNewMessage("");
      refresh();
      setPendingMessage(content);
      router.push(`/chat?id=${chat.id}`);
    } finally {
      setSubmitting(false);
    }
  };

  if (!space) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <p className="text-text-tertiary text-sm">Space not found.</p>
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col">
      <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col">
        <div className="border-b border-border px-6 py-4">
          <h2 className="text-2xl font-bold text-text-primary">{space.name}</h2>
        </div>

        <div className="px-6 py-4">
          <form onSubmit={handleNewChat}>
            <div className="flex items-center gap-2 rounded-xl border border-border bg-surface-secondary px-3 py-2 focus-within:border-accent transition-colors">
              <textarea
                autoFocus
                value={newMessage}
                onChange={(e) => setNewMessage(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    handleNewChat(e);
                  }
                }}
                placeholder="Send a message to start a new chat..."
                rows={1}
                disabled={submitting}
                className="flex-1 resize-none bg-transparent text-sm leading-5 text-text-primary placeholder:text-text-tertiary focus:outline-none m-0 p-0"
              />
              <button
                type="submit"
                disabled={!newMessage.trim() || submitting}
                className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary disabled:opacity-30 transition"
              >
                <PaperAirplaneIcon className="h-5 w-5" />
              </button>
            </div>
          </form>
        </div>

        <div className="flex-1 overflow-y-auto px-6">
          {space.chats.length > 0 && (
            <div className="space-y-1">
              <p className="text-[11px] font-semibold uppercase tracking-wider text-text-tertiary pb-1">
                Chats
              </p>
              {space.chats.map((chat) => (
                <button
                  key={chat.id}
                  onClick={() => router.push(`/chat?id=${chat.id}`)}
                  className={`w-full rounded-lg px-4 py-2.5 text-left text-sm transition truncate ${
                    activeChatId === chat.id
                      ? "bg-surface-tertiary text-text-primary"
                      : "text-text-secondary hover:bg-surface-secondary"
                  }`}
                >
                  {chat.title ?? "New chat"}
                </button>
              ))}
            </div>
          )}
          {space.chats.length === 0 && (
            <p className="py-8 text-center text-sm text-text-tertiary">
              No chats in this space yet. Type above to start one.
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

const statusColors: Record<string, string> = {
  running: "bg-success",
  serving: "bg-success",
  starting: "bg-warning",
  hibernated: "bg-warning",
  stopped: "bg-text-tertiary",
  failed: "bg-danger",
};

function AppCard({
  app,
  onUpdate,
  onDelete,
}: {
  app: AppResponse;
  onUpdate: (app: AppResponse) => void;
  onDelete: (id: string) => void;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const [acting, setActing] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [menuOpen]);

  const handleStop = async () => {
    setActing(true);
    try {
      const updated = await api.post<AppResponse>(`/api/apps/${app.id}/stop`, {});
      onUpdate(updated);
    } catch {}
    setActing(false);
    setMenuOpen(false);
  };

  const handleDelete = async () => {
    setActing(true);
    try {
      await api.delete(`/api/apps/${app.id}`);
      onDelete(app.id);
    } catch {}
    setActing(false);
    setMenuOpen(false);
  };

  const icon = app.manifest?.icon as string | undefined;
  const href = app.url ? `${API_URL}${app.url}` : undefined;

  return (
    <div className="relative flex items-center gap-3 rounded-lg border border-border px-4 py-3 transition hover:border-accent">
      {icon ? (
        <span className="text-lg shrink-0">{icon}</span>
      ) : (
        <span
          className={`h-2 w-2 shrink-0 rounded-full ${statusColors[app.status] || "bg-text-tertiary"}`}
        />
      )}
      <a
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className={`min-w-0 flex-1 ${href ? "cursor-pointer" : "cursor-default opacity-60"}`}
      >
        <p className="text-sm font-medium text-text-primary truncate">
          {app.name}
        </p>
        {app.description && (
          <p className="text-xs text-text-tertiary truncate">
            {app.description}
          </p>
        )}
      </a>
      <span className="text-[10px] text-text-tertiary capitalize shrink-0">
        {app.status}
      </span>
      <div ref={menuRef} className="relative shrink-0">
        <button
          onClick={() => setMenuOpen((o) => !o)}
          className="rounded p-1 text-text-tertiary hover:text-text-primary hover:bg-surface-tertiary transition"
        >
          <EllipsisVerticalIcon className="h-4 w-4" />
        </button>
        {menuOpen && (
          <div className="absolute right-0 top-full z-10 mt-1 w-32 rounded-lg border border-border bg-surface-secondary py-1 shadow-lg">
            {(app.status === "running" || app.status === "serving") && (
              <button
                onClick={handleStop}
                disabled={acting}
                className="w-full px-3 py-1.5 text-left text-xs text-text-secondary hover:bg-surface-tertiary transition"
              >
                Stop
              </button>
            )}
            <button
              onClick={handleDelete}
              disabled={acting}
              className="w-full px-3 py-1.5 text-left text-xs text-danger hover:bg-surface-tertiary transition"
            >
              Delete
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function LandingView() {
  const { createChat, setPendingMessage } = useSession();
  const { addStandaloneChat } = useNavigation();
  const router = useRouter();
  const [newMessage, setNewMessage] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [apps, setApps] = useState<AppResponse[]>([]);

  useEffect(() => {
    api.get<AppResponse[]>("/api/apps").then(setApps).catch(() => {});
  }, []);

  const handleNewChat = async (e: React.FormEvent) => {
    e.preventDefault();
    const content = newMessage.trim();
    if (!content || submitting) return;
    setSubmitting(true);
    try {
      const chat = await createChat({ agent_id: "system" });
      addStandaloneChat(chat);
      setNewMessage("");
      setPendingMessage(content);
      router.push(`/chat?id=${chat.id}`);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="flex min-h-full flex-col items-center justify-center px-8">
      <div className="w-full max-w-3xl space-y-8">
        <form onSubmit={handleNewChat}>
          <div className="flex items-center gap-2 rounded-xl border border-border bg-surface-secondary px-3 py-2 focus-within:border-accent transition-colors">
            <textarea
              autoFocus
              value={newMessage}
              onChange={(e) => setNewMessage(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  handleNewChat(e);
                }
              }}
              placeholder="Send a message to start a new chat..."
              rows={1}
              disabled={submitting}
              className="flex-1 resize-none bg-transparent text-sm leading-5 text-text-primary placeholder:text-text-tertiary focus:outline-none m-0 p-0"
            />
            <button
              type="submit"
              disabled={!newMessage.trim() || submitting}
              className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary disabled:opacity-30 transition"
            >
              <PaperAirplaneIcon className="h-5 w-5" />
            </button>
          </div>
        </form>

        {apps.length > 0 && (
          <div>
            <p className="text-[11px] font-semibold uppercase tracking-wider text-text-tertiary pb-2">
              Services
            </p>
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
              {apps.map((app) => (
                <AppCard
                  key={app.id}
                  app={app}
                  onUpdate={(updated) =>
                    setApps((prev) =>
                      prev.map((a) => (a.id === updated.id ? updated : a)),
                    )
                  }
                  onDelete={(id) =>
                    setApps((prev) => prev.filter((a) => a.id !== id))
                  }
                />
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export default function ChatPage() {
  const searchParams = useSearchParams();
  const { activeChatId, activeTaskId } = useSession();
  const spaceId = searchParams.get("space");

  if (activeChatId || activeTaskId) {
    return null;
  }

  if (spaceId) {
    return <SpaceView spaceId={spaceId} />;
  }

  return <LandingView />;
}
