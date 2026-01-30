"use client";

import { useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { PaperAirplaneIcon } from "@heroicons/react/24/solid";
import { api } from "@/lib/api-client";
import { useChat } from "@/lib/chat-context";
import { useNavigation } from "@/lib/navigation-context";
import type { ChatResponse } from "@/lib/types";

function SpaceView({ spaceId }: { spaceId: string }) {
  const { spaces, refresh } = useNavigation();
  const { activeChatId, setPendingMessage } = useChat();
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

function LandingView() {
  const { createChat, setPendingMessage } = useChat();
  const { addStandaloneChat } = useNavigation();
  const router = useRouter();
  const [newMessage, setNewMessage] = useState("");
  const [submitting, setSubmitting] = useState(false);

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
      <div className="w-full max-w-3xl">
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
    </div>
  );
}

export default function ChatPage() {
  const searchParams = useSearchParams();
  const { activeChatId } = useChat();
  const spaceId = searchParams.get("space");

  if (activeChatId) {
    return null;
  }

  if (spaceId) {
    return <SpaceView spaceId={spaceId} />;
  }

  return <LandingView />;
}
