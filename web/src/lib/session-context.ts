"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  createElement,
} from "react";
import { useSearchParams } from "next/navigation";
import { api, getTask } from "./api-client";
import { sseBus, type GlobalSSEEvent } from "./sse-event-bus";
import { useNavigation, useSystemAgent } from "./navigation-context";
import { useNotifications } from "./notification-context";
import type { Attachment, ChatResponse, CreateChatRequest, TaskResponse } from "./types";


interface PendingMessage {
  content: string;
  attachments?: Attachment[];
}

const sessionStore = {
  activeTaskId: null as string | null,
  pendingMessage: null as PendingMessage | null,
};


interface SessionContextValue {
  activeChatId: string | null;
  activeChat: ChatResponse | null;
  setActiveChat: (chat: ChatResponse | null) => void;
  activeTaskId: string | null;
  activeTask: TaskResponse | null;
  agentId: string | null;
  createChat: (req: CreateChatRequest) => Promise<ChatResponse>;
  setPendingMessage: (message: string, attachments?: Attachment[]) => void;
  getPendingMessage: () => PendingMessage | null;
}

const SessionContext = createContext<SessionContextValue | null>(null);

export function SessionProvider({ children }: { children: React.ReactNode }) {
  const searchParams = useSearchParams();
  const chatIdParam = searchParams.get("id");
  const taskIdParam = searchParams.get("task");
  const agentParam = searchParams.get("agent");
  const [activeTask, setActiveTask] = useState<TaskResponse | null>(null);
  const activeChatId = chatIdParam ?? activeTask?.chat_id ?? null;
  const activeTaskId = taskIdParam;
  const [activeChat, setActiveChat] = useState<ChatResponse | null>(null);
  const systemAgent = useSystemAgent();
  const agentId = activeChat?.agent_id ?? agentParam ?? systemAgent.id;
  const { updateChatTitle, updateAgent, updateTaskInList, setActiveTab, standaloneChats, spaces, archivedChats } = useNavigation();
  const { addNotification } = useNotifications();

  const [prevActiveChatId, setPrevActiveChatId] = useState(activeChatId);
  const [prevAgentParam, setPrevAgentParam] = useState(agentParam);
  if (activeChatId !== prevActiveChatId) {
    setPrevActiveChatId(activeChatId);
    if (!activeChatId) {
      setActiveChat(null);
    } else {
      const found = standaloneChats.find(c => c.id === activeChatId)
        ?? spaces.flatMap(s => s.chats).find(c => c.id === activeChatId)
        ?? archivedChats.find(c => c.id === activeChatId)
        ?? null;
      setActiveChat(found);
    }
  }
  if (agentParam !== prevAgentParam) {
    setPrevAgentParam(agentParam);
    if (!agentParam && !activeChatId) {
      setActiveChat(null);
    }
  }

  const [prevTaskId, setPrevTaskId] = useState(taskIdParam);
  if (taskIdParam !== prevTaskId) {
    setPrevTaskId(taskIdParam);
    if (!taskIdParam) {
      setActiveTask(null);
    }
  }

  useEffect(() => {
    sessionStore.activeTaskId = taskIdParam;
  }, [taskIdParam]);

  useEffect(() => {
    if (taskIdParam) {
      setActiveTab("tasks");
    }
  }, [taskIdParam, setActiveTab]);

  useEffect(() => {
    if (!taskIdParam) return;
    let cancelled = false;
    getTask(taskIdParam)
      .then((t) => { if (!cancelled) setActiveTask(t); })
      .catch(() => { if (!cancelled) setActiveTask(null); });
    return () => { cancelled = true; };
  }, [taskIdParam]);

  // Fallback for deep-links not yet resolved from navigation context.
  useEffect(() => {
    if (!activeChatId || activeChat?.id === activeChatId) return;
    let cancelled = false;
    api.get<ChatResponse>(`/api/chats/${activeChatId}`)
      .then((chat) => { if (!cancelled) setActiveChat(chat); })
      .catch(() => { if (!cancelled) setActiveChat(null); });
    return () => { cancelled = true; };
  }, [activeChatId, activeChat]);

  useEffect(() => {
    const controller = new AbortController();
    sseBus.connect(controller.signal);

    const unsubscribe = sseBus.onGlobal((event: GlobalSSEEvent) => {
      switch (event.type) {
        case "title":
          setActiveChat((prev) =>
            prev && prev.id === event.chatId ? { ...prev, title: event.title } : prev,
          );
          updateChatTitle(event.chatId, event.title);
          break;
        case "entity_updated":
          if (event.table === "agent") {
            updateAgent(event.recordId, event.fields);
          }
          break;
        case "task_update":
          updateTaskInList(event.taskId, {
            status: event.status,
            title: event.title,
            chat_id: event.chatId,
            result_summary: event.resultSummary,
          });
          if (event.taskId && sessionStore.activeTaskId && event.taskId === sessionStore.activeTaskId) {
            getTask(event.taskId)
              .then((t) => setActiveTask(t))
              .catch(() => {});
          }
          break;
        case "inference_count":
        case "activity_changed":
          break;
        case "notification":
          addNotification(event.notification);
          break;
      }
    });

    return () => {
      unsubscribe();
      controller.abort();
    };
  }, [updateChatTitle, updateAgent, updateTaskInList, addNotification]);

  const setPendingMessage = useCallback((message: string, attachments?: Attachment[]) => {
    sessionStore.pendingMessage = { content: message, attachments };
  }, []);

  const getPendingMessage = useCallback(() => {
    const msg = sessionStore.pendingMessage;
    sessionStore.pendingMessage = null;
    return msg;
  }, []);

  const createChat = useCallback(async (req: CreateChatRequest) => {
    return await api.post<ChatResponse>("/api/chats", req);
  }, []);

  return createElement(
    SessionContext.Provider,
    {
      value: {
        activeChatId,
        activeChat,
        setActiveChat,
        activeTaskId,
        activeTask,
        agentId,
        createChat,
        setPendingMessage,
        getPendingMessage,
      },
    },
    children,
  );
}

export function useSession(): SessionContextValue {
  const ctx = useContext(SessionContext);
  if (!ctx) throw new Error("useSession must be used within SessionProvider");
  return ctx;
}
