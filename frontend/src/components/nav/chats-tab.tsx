"use client";

import { useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { FolderPlusIcon, PlusIcon } from "@heroicons/react/24/outline";
import { api } from "@/lib/api-client";
import { useNavigation } from "@/lib/navigation-context";
import { useChat } from "@/lib/chat-context";
import type { SpaceResponse } from "@/lib/types";

export function ChatsTab() {
  const { spaces, standaloneChats, refresh, addStandaloneChat } = useNavigation();
  const { activeChatId, createChat } = useChat();
  const router = useRouter();
  const searchParams = useSearchParams();
  const activeSpaceId = searchParams.get("space");
  const [creatingSpace, setCreatingSpace] = useState(false);
  const [spaceName, setSpaceName] = useState("");

  const handleNewChat = async () => {
    const chat = await createChat({ agent_id: "system" });
    addStandaloneChat(chat);
    router.push(`/chat?id=${chat.id}`);
  };

  const handleCreateSpace = async (e: React.FormEvent) => {
    e.preventDefault();
    const name = spaceName.trim();
    if (!name) return;
    await api.post<SpaceResponse>("/api/spaces", { name });
    setSpaceName("");
    setCreatingSpace(false);
    refresh();
  };

  return (
    <div className="space-y-1 p-2">
      <div className="flex items-center justify-between px-2 pb-1">
        <span className="text-[10px] font-semibold uppercase tracking-wider text-text-tertiary">
          Spaces
        </span>
        <button
          onClick={() => setCreatingSpace((v) => !v)}
          className="rounded p-0.5 text-text-tertiary hover:text-text-primary transition"
          title="New space"
        >
          <FolderPlusIcon className="h-3.5 w-3.5" />
        </button>
      </div>

      {creatingSpace && (
        <form onSubmit={handleCreateSpace} className="px-2 pb-1">
          <input
            autoFocus
            value={spaceName}
            onChange={(e) => setSpaceName(e.target.value)}
            onBlur={() => {
              if (!spaceName.trim()) setCreatingSpace(false);
            }}
            placeholder="Space name..."
            className="w-full rounded-lg border border-border bg-surface px-2 py-1 text-sm text-text-primary placeholder:text-text-tertiary focus:outline-none focus:border-text-secondary"
          />
        </form>
      )}

      {spaces.map((space) => (
        <button
          key={space.id}
          onClick={() => router.push(`/chat?space=${space.id}`)}
          className={`flex w-full items-center gap-1.5 rounded-lg px-3 py-2 text-sm font-medium transition ${
            activeSpaceId === space.id
              ? "bg-surface-tertiary text-text-primary"
              : "text-text-primary hover:bg-surface-secondary"
          }`}
        >
          <span className="truncate">{space.name}</span>
          <span className="ml-auto text-[10px] text-text-tertiary">
            {space.chats.length}
          </span>
        </button>
      ))}

      {standaloneChats.length > 0 && (
        <div className="pt-2">
          <div className="flex items-center justify-between px-2 pb-1">
            <span className="text-[10px] font-semibold uppercase tracking-wider text-text-tertiary">
              Chats
            </span>
            <button
              onClick={handleNewChat}
              className="rounded p-0.5 text-text-tertiary hover:text-text-primary transition"
              title="New chat"
            >
              <PlusIcon className="h-3.5 w-3.5" />
            </button>
          </div>
          {standaloneChats.map((chat) => (
            <button
              key={chat.id}
              onClick={() => router.push(`/chat?id=${chat.id}`)}
              className={`w-full rounded-lg px-3 py-2 text-left text-sm transition truncate ${
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
      {spaces.length === 0 && standaloneChats.length === 0 && !creatingSpace && (
        <p className="px-2 py-4 text-center text-xs text-text-tertiary">
          No chats yet. Start a new conversation!
        </p>
      )}
    </div>
  );
}
