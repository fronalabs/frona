"use client";

import { useCallback, useMemo, useState, useEffect, useRef } from "react";
import { useRouter } from "next/navigation";
import { AssistantRuntimeProvider } from "@assistant-ui/react";
import { useSession } from "@/lib/session-context";
import { useNavigation, useSystemAgent } from "@/lib/navigation-context";
import { useNotifications } from "@/lib/notification-context";
import { ChatProvider } from "@/lib/chat-context";
import { useChatRuntime } from "@/lib/use-chat-runtime";
import { RetryContext } from "@/lib/retry-context";
import { PendingToolsContext } from "@/lib/pending-tools-context";
import { ChatPaginationContext } from "@/lib/chat-pagination-context";
import { ChatHeader } from "./chat-header";
import { TaskHeader } from "./task-header";
import { AssistantThread } from "./assistant-thread";
import { ToolUIRegistry } from "./tool-uis";
import { CronRunsTable } from "./cron-runs-table";
import type { ChatResponse } from "@/lib/types";

export function ChatView({
  chatId,
  agentId,
  onChatPromoted,
  hideHeader = false,
}: {
  chatId?: string;
  agentId: string;
  onChatPromoted?: (chatId: string) => void;
  /** When true, ChatView does NOT render its own `ChatHeader` — used in
   *  task mode where the panel shows `TaskHeader` instead. */
  hideHeader?: boolean;
}) {
  const { setActiveChat, getPendingMessage } = useSession();
  const { addStandaloneChat } = useNavigation();
  const [currentChatId, setCurrentChatId] = useState<string | null>(chatId ?? null);

  const onChatCreated = useCallback((chat: ChatResponse) => {
    setCurrentChatId(chat.id);
    addStandaloneChat(chat);
    setActiveChat(chat);
    onChatPromoted?.(chat.id);
  }, [addStandaloneChat, setActiveChat, onChatPromoted]);

  const { runtime, loaded, sendMessage, retryInfo, pendingTools, hasMore, loadingMore, loadOlder, usagePerChat, lastFallbackIndex, lastChatInputTokens, totalToolCalls } = useChatRuntime({ chatId, agentId, onChatCreated });
  const pagination = useMemo(
    () => ({ hasMore, loadingMore, loadOlder }),
    [hasMore, loadingMore, loadOlder],
  );

  const pendingHandled = useRef(false);
  useEffect(() => {
    if (!loaded || pendingHandled.current) return;
    pendingHandled.current = true;
    const pending = getPendingMessage();
    if (pending) {
      sendMessage(pending.content, pending.attachments);
    }
  }, [loaded, sendMessage, getPendingMessage]);

  const content = (
    <>
      {!hideHeader && (
        <div className="mx-auto w-full max-w-3xl">
          <ChatHeader
            chatId={currentChatId ?? undefined}
            agentId={agentId}
            totals={usagePerChat}
            lastFallbackIndex={lastFallbackIndex}
            lastChatInputTokens={lastChatInputTokens}
            totalToolCalls={totalToolCalls}
          />
        </div>
      )}
      <ToolUIRegistry />
      {loaded ? <AssistantThread /> : <div className="flex-1" />}
    </>
  );

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <PendingToolsContext value={pendingTools}>
      <ChatPaginationContext value={pagination}>
      <RetryContext value={retryInfo}>
        {currentChatId ? (
          <ChatProvider chatId={currentChatId} agentId={agentId}>
            {content}
          </ChatProvider>
        ) : (
          content
        )}
      </RetryContext>
      </ChatPaginationContext>
      </PendingToolsContext>
    </AssistantRuntimeProvider>
  );
}

const MAX_MOUNTED_CHATS = 20;

/**
 * Each slot has a stable `slotId` used as the React key so that a pending
 * ChatView can be promoted to a real chat without remounting the component.
 * `chatId` starts null for pending slots and is filled when the adapter
 * creates the chat on first message.
 */
interface ChatSlot {
  slotId: string;
  chatId: string | null;
  agentId: string;
  lastActiveAt: number;
}

export function ConversationPanel() {
  const router = useRouter();
  const { activeChatId, activeChat, activeTask, agentId } = useSession();
  const { markReadByChat } = useNotifications();
  const systemAgent = useSystemAgent();

  const [pendingSessionId, setPendingSessionId] = useState(0);
  const [prevActiveChat, setPrevActiveChat] = useState(activeChat);
  const [slots, setSlots] = useState<ChatSlot[]>(() =>
    activeChatId
      ? [{ slotId: activeChatId, chatId: activeChatId, agentId: agentId ?? systemAgent.id, lastActiveAt: Date.now() }]
      : [],
  );

  if (activeChat !== prevActiveChat) {
    setPrevActiveChat(activeChat);
    if (!activeChat && prevActiveChat) {
      setPendingSessionId((n) => n + 1);
    }
  }

  const effectiveAgentId = agentId ?? systemAgent.id;

  const [prevActiveChatId, setPrevActiveChatId] = useState(activeChatId);
  if (activeChatId !== prevActiveChatId) {
    setPrevActiveChatId(activeChatId);
    if (activeChatId) {
      setSlots((prev) => {
        const existing = prev.find((c) => c.chatId === activeChatId);
        if (existing) {
          return prev.map((c) =>
            c.chatId === activeChatId ? { ...c, lastActiveAt: Date.now() } : c,
          );
        }
        let next = [...prev, { slotId: activeChatId, chatId: activeChatId, agentId: effectiveAgentId, lastActiveAt: Date.now() }];
        if (next.length > MAX_MOUNTED_CHATS) {
          next = next
            .sort((a, b) => b.lastActiveAt - a.lastActiveAt)
            .filter((c, i) => i < MAX_MOUNTED_CHATS || c.chatId === activeChatId);
        }
        return next;
      });
    }
  }

  useEffect(() => {
    if (activeChatId) markReadByChat(activeChatId);
  }, [activeChatId, markReadByChat]);

  const pendingSlotId = !activeChatId ? `pending-${pendingSessionId}` : null;
  if (pendingSlotId && !slots.some((s) => s.slotId === pendingSlotId)) {
    setSlots((prev) => [...prev, { slotId: pendingSlotId, chatId: null, agentId: effectiveAgentId, lastActiveAt: Date.now() }]);
  }

  // Updates slot in place (no remount) and navigates to the chat URL.
  const promotePendingSlot = useCallback((slotId: string, chatId: string) => {
    setSlots((prev) => prev.map((s) =>
      s.slotId === slotId ? { ...s, chatId, lastActiveAt: Date.now() } : s,
    ));
    router.replace(`/chat?id=${chatId}`, { scroll: false });
  }, [router]);

  if (activeTask) {
    const isCronTemplate = activeTask.kind.type === "Cron";
    return (
      <div className="flex-1 overflow-hidden bg-surface flex flex-col min-w-0">
        <div className="mx-auto w-full max-w-3xl">
          <TaskHeader />
        </div>
        {isCronTemplate ? (
          <CronRunsTable cronId={activeTask.id} task={activeTask} />
        ) : activeChatId ? (
          <ChatView key={activeChatId} chatId={activeChatId} agentId={effectiveAgentId} hideHeader />
        ) : (
          <div className="flex flex-1 items-center justify-center">
            <p className="text-sm text-text-tertiary">Task has not started yet.</p>
          </div>
        )}
      </div>
    );
  }

  const isActive = (s: ChatSlot) =>
    (s.chatId != null && s.chatId === activeChatId) ||
    (s.chatId == null && !activeChatId && s.slotId === pendingSlotId);

  return (
    <div className="flex-1 overflow-hidden bg-surface flex flex-col min-w-0">
      <div className="relative flex flex-1 flex-col min-h-0">
        {slots.map((s) => (
          <div
            key={s.slotId}
            className={
              isActive(s)
                ? "flex flex-1 flex-col min-h-0"
                : "absolute inset-0 flex flex-col invisible"
            }
          >
            <ChatView
              chatId={s.chatId ?? undefined}
              agentId={s.agentId}
              onChatPromoted={s.chatId == null ? (chatId) => promotePendingSlot(s.slotId, chatId) : undefined}
            />
          </div>
        ))}
      </div>
    </div>
  );
}
