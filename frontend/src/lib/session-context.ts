"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  createElement,
  useRef,
} from "react";
import { useSearchParams, useRouter } from "next/navigation";
import { api, streamMessage, streamSession, cancelGeneration, getTask } from "./api-client";
import { useNavigation } from "./navigation-context";
import type { ChatResponse, MessageResponse, CreateChatRequest, ToolCallStatus, TaskResponse, Attachment } from "./types";


interface SessionContextValue {
  activeChatId: string | null;
  activeChat: ChatResponse | null;
  activeTaskId: string | null;
  activeTask: TaskResponse | null;
  pendingAgentId: string | null;
  messages: MessageResponse[];
  sending: boolean;
  inferring: boolean;
  streamingContent: string | null;
  activeToolCalls: ToolCallStatus[];
  sendMessage: (content: string, attachments?: Attachment[]) => Promise<void>;
  stopGeneration: () => void;
  createChat: (req: CreateChatRequest) => Promise<ChatResponse>;
  setPendingMessage: (message: string) => void;
  resolveToolMessage: (messageId: string, response?: string) => Promise<void>;
}

const SessionContext = createContext<SessionContextValue | null>(null);

export function SessionProvider({ children }: { children: React.ReactNode }) {
  const searchParams = useSearchParams();
  const router = useRouter();
  const chatIdParam = searchParams.get("id");
  const taskIdParam = searchParams.get("task");
  const agentParam = searchParams.get("agent");
  const [activeTask, setActiveTask] = useState<TaskResponse | null>(null);
  const activeChatId = chatIdParam ?? activeTask?.chat_id ?? null;
  const activeTaskId = taskIdParam;
  const pendingAgentId = !activeChatId ? agentParam : null;
  const [activeChat, setActiveChat] = useState<ChatResponse | null>(null);
  const [messages, setMessages] = useState<MessageResponse[]>([]);
  const [sending, setSending] = useState(false);
  const [inferring, setInferring] = useState(false);
  const [streamingContent, setStreamingContent] = useState<string | null>(null);
  const streamingContentRef = useRef<string>("");
  const [activeToolCalls, setActiveToolCalls] = useState<ToolCallStatus[]>([]);
  const activeToolCallsRef = useRef<ToolCallStatus[]>([]);
  const abortControllerRef = useRef<AbortController | null>(null);
  const pendingMessageRef = useRef<string | null>(null);
  const activeChatIdRef = useRef<string | null>(null);
  const activeTaskIdRef = useRef<string | null>(null);
  const { updateChatTitle, updateAgent, addStandaloneChat, updateTaskInList, setActiveTab } = useNavigation();

  useEffect(() => {
    activeChatIdRef.current = activeChatId;
  }, [activeChatId]);

  useEffect(() => {
    activeTaskIdRef.current = taskIdParam;
  }, [taskIdParam]);

  useEffect(() => {
    if (taskIdParam) {
      setActiveTab("tasks");
    }
  }, [taskIdParam, setActiveTab]);

  useEffect(() => {
    if (!taskIdParam) {
      setActiveTask(null);
      return;
    }
    let cancelled = false;
    getTask(taskIdParam)
      .then((t) => { if (!cancelled) setActiveTask(t); })
      .catch(() => { if (!cancelled) setActiveTask(null); });
    return () => { cancelled = true; };
  }, [taskIdParam]);

  const setPendingMessage = useCallback((message: string) => {
    pendingMessageRef.current = message;
  }, []);

  useEffect(() => {
    const controller = new AbortController();

    streamSession(
      {
        onChatMessage: (chatId, message) => {
          if (chatId !== activeChatIdRef.current) return;
          setMessages((prev) => {
            const idx = prev.findIndex((m) => m.id === message.id);
            if (idx >= 0) {
              const updated = [...prev];
              updated[idx] = message;
              return updated;
            }
            return [...prev, message];
          });
        },
        onTaskUpdate: (taskId, status, sourceChatId, title, chatId, resultSummary) => {
          updateTaskInList(taskId, { status, title, chat_id: chatId, result_summary: resultSummary });
          if (
            sourceChatId &&
            sourceChatId === activeChatIdRef.current &&
            (status === "completed" || status === "failed")
          ) {
            api
              .get<MessageResponse[]>(`/api/chats/${sourceChatId}/messages`)
              .then((msgs) => setMessages(msgs))
              .catch(() => {});
          }
          if (taskId && activeTaskIdRef.current && taskId === activeTaskIdRef.current) {
            getTask(taskId)
              .then((t) => setActiveTask(t))
              .catch(() => {});
          }
        },
        onInferenceCount: (count) => {
          setInferring(count > 0);
        },
      },
      controller.signal,
    );

    return () => {
      controller.abort();
    };
  }, [updateTaskInList]);

  useEffect(() => {
    if (!activeChatId) {
      setActiveChat(null);
      setMessages([]);
      return;
    }

    let cancelled = false;

    async function load() {
      try {
        const [chat, msgs] = await Promise.all([
          api.get<ChatResponse>(`/api/chats/${activeChatId}`),
          api.get<MessageResponse[]>(`/api/chats/${activeChatId}/messages`),
        ]);
        if (!cancelled) {
          setActiveChat(chat);
          setMessages(msgs);
        }
      } catch {
        if (!cancelled) {
          setActiveChat(null);
          setMessages([]);
        }
      }
    }

    load();
    return () => {
      cancelled = true;
    };
  }, [activeChatId]);

  const resolveToolMessage = useCallback(
    async (messageId: string, response?: string) => {
      if (!activeChatId) return;
      const updated = await api.post<MessageResponse>(
        `/api/chats/${activeChatId}/messages/${messageId}/resolve`,
        { response: response ?? null },
      );
      setMessages((prev) =>
        prev.map((m) => (m.id === messageId ? updated : m)),
      );
    },
    [activeChatId],
  );

  const sendMessage = useCallback(
    async (content: string, attachments?: Attachment[]) => {
      if (!activeChatId && pendingAgentId) {
        const chat = await api.post<ChatResponse>("/api/chats", { agent_id: pendingAgentId });
        addStandaloneChat(chat);
        pendingMessageRef.current = content;
        router.push(`/chat?id=${chat.id}`);
        return;
      }
      if (!activeChatId) return;
      setSending(true);
      streamingContentRef.current = "";
      setStreamingContent("");
      activeToolCallsRef.current = [];
      setActiveToolCalls([]);

      const controller = new AbortController();
      abortControllerRef.current = controller;

      const body = attachments?.length
        ? { content, attachments }
        : { content };

      await streamMessage(activeChatId, body, {
        onUserMessage: (msg) => {
          setMessages((prev) => [...prev, msg]);
        },
        onToken: (tokenContent) => {
          streamingContentRef.current += tokenContent;
          setStreamingContent(streamingContentRef.current);
        },
        onDone: (msg) => {
          setStreamingContent(null);
          activeToolCallsRef.current = [];
          setActiveToolCalls([]);
          setMessages((prev) => [...prev, msg]);
          setSending(false);
          abortControllerRef.current = null;
        },
        onError: () => {
          setStreamingContent(null);
          activeToolCallsRef.current = [];
          setActiveToolCalls([]);
          setSending(false);
          abortControllerRef.current = null;
        },
        onTitle: (title) => {
          setActiveChat((prev) => (prev ? { ...prev, title } : prev));
          updateChatTitle(activeChatId, title);
        },
        onToolCall: (name, _args, description) => {
          if (name === "ask_user_question" || name === "request_user_takeover") return;
          const entry: ToolCallStatus = {
            name,
            description: description ?? null,
            status: "running",
          };
          activeToolCallsRef.current = [...activeToolCallsRef.current, entry];
          setActiveToolCalls(activeToolCallsRef.current);
        },
        onToolResult: (name) => {
          const idx = activeToolCallsRef.current.findIndex(
            (tc) => tc.name === name && tc.status === "running",
          );
          if (idx !== -1) {
            const updated = [...activeToolCallsRef.current];
            updated[idx] = { ...updated[idx], status: "done" };
            activeToolCallsRef.current = updated;
            setActiveToolCalls(activeToolCallsRef.current);
          }
        },
        onToolMessage: (msg) => {
          setMessages((prev) => [...prev, msg]);
          setStreamingContent(null);
          activeToolCallsRef.current = [];
          setActiveToolCalls([]);
          setSending(false);
        },
        onToolResolved: (msg) => {
          setMessages((prev) =>
            prev.map((m) => (m.id === msg.id ? msg : m)),
          );
        },
        onRateLimit: (retryAfterSecs) => {
          const entry: ToolCallStatus = {
            name: "rate_limit",
            description: `Rate limited, retrying in ${retryAfterSecs}s...`,
            status: "running",
          };
          activeToolCallsRef.current = [entry];
          setActiveToolCalls(activeToolCallsRef.current);
        },
        onEntityUpdated: (table, recordId, fields) => {
          if (table === "agent") {
            updateAgent(recordId, fields);
          }
        },
        onCancelled: () => {
          setStreamingContent(null);
          activeToolCallsRef.current = [];
          setActiveToolCalls([]);
          setSending(false);
          abortControllerRef.current = null;
          api.get<MessageResponse[]>(
            `/api/chats/${activeChatId}/messages`,
          ).then((msgs) => setMessages(msgs)).catch(() => {});
        },
        onStreamEnd: async () => {
          try {
            const msgs = await api.get<MessageResponse[]>(
              `/api/chats/${activeChatId}/messages`,
            );
            setMessages(msgs);
          } catch {
            // ignore
          }
          setStreamingContent(null);
          activeToolCallsRef.current = [];
          setActiveToolCalls([]);
          setSending(false);
        },
      }, controller.signal);
    },
    [activeChatId, pendingAgentId, router, addStandaloneChat, updateChatTitle, updateAgent],
  );

  useEffect(() => {
    if (!activeChatId || !pendingMessageRef.current) return;
    const content = pendingMessageRef.current;
    pendingMessageRef.current = null;
    sendMessage(content);
  }, [activeChatId, sendMessage]);

  const stopGeneration = useCallback(() => {
    if (!activeChatId) return;
    abortControllerRef.current?.abort();
    abortControllerRef.current = null;
    cancelGeneration(activeChatId).catch(() => {});
    setStreamingContent(null);
    activeToolCallsRef.current = [];
    setActiveToolCalls([]);
    setSending(false);
    api.get<MessageResponse[]>(
      `/api/chats/${activeChatId}/messages`,
    ).then((msgs) => setMessages(msgs)).catch(() => {});
  }, [activeChatId]);

  const createChat = useCallback(async (req: CreateChatRequest) => {
    return await api.post<ChatResponse>("/api/chats", req);
  }, []);

  return createElement(
    SessionContext.Provider,
    {
      value: {
        activeChatId,
        activeChat,
        activeTaskId,
        activeTask,
        pendingAgentId,
        messages,
        sending,
        inferring,
        streamingContent,
        activeToolCalls,
        sendMessage,
        stopGeneration,
        createChat,
        setPendingMessage,
        resolveToolMessage,
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
