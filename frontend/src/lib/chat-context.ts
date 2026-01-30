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
import { useSearchParams } from "next/navigation";
import { api, streamMessage, cancelGeneration } from "./api-client";
import { useNavigation } from "./navigation-context";
import type { ChatResponse, MessageResponse, CreateChatRequest, ToolCallStatus } from "./types";

const API_URL = process.env.NEXT_PUBLIC_API_URL || "http://localhost:3001";

interface ChatContextValue {
  activeChatId: string | null;
  activeChat: ChatResponse | null;
  messages: MessageResponse[];
  sending: boolean;
  streamingContent: string | null;
  activeToolCalls: ToolCallStatus[];
  sendMessage: (content: string) => Promise<void>;
  stopGeneration: () => void;
  createChat: (req: CreateChatRequest) => Promise<ChatResponse>;
  setPendingMessage: (message: string) => void;
  resolveToolMessage: (messageId: string, response?: string) => Promise<void>;
}

const ChatContext = createContext<ChatContextValue | null>(null);

export function ChatProvider({ children }: { children: React.ReactNode }) {
  const searchParams = useSearchParams();
  const activeChatId = searchParams.get("id");
  const [activeChat, setActiveChat] = useState<ChatResponse | null>(null);
  const [messages, setMessages] = useState<MessageResponse[]>([]);
  const [sending, setSending] = useState(false);
  const [streamingContent, setStreamingContent] = useState<string | null>(null);
  const streamingContentRef = useRef<string>("");
  const [activeToolCalls, setActiveToolCalls] = useState<ToolCallStatus[]>([]);
  const activeToolCallsRef = useRef<ToolCallStatus[]>([]);
  const abortControllerRef = useRef<AbortController | null>(null);
  const pendingMessageRef = useRef<string | null>(null);
  const { updateChatTitle, updateAgent } = useNavigation();

  const setPendingMessage = useCallback((message: string) => {
    pendingMessageRef.current = message;
  }, []);

  useEffect(() => {
    const token = localStorage.getItem("token");
    if (!token) return;

    const es = new EventSource(
      `${API_URL}/api/chats/stream?token=${token}`,
    );

    es.addEventListener("chat_message", (event) => {
      try {
        const data = JSON.parse(event.data);
        const chatId = data.chat_id as string;
        const message = data.message as MessageResponse;
        setMessages((prev) => {
          if (prev.length > 0 && prev[0].chat_id === chatId) {
            return [...prev, message];
          }
          return prev;
        });
      } catch {
        // skip malformed data
      }
    });

    return () => {
      es.close();
    };
  }, []);

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
    async (content: string) => {
      if (!activeChatId) return;
      setSending(true);
      streamingContentRef.current = "";
      setStreamingContent("");
      activeToolCallsRef.current = [];
      setActiveToolCalls([]);

      const controller = new AbortController();
      abortControllerRef.current = controller;

      await streamMessage(activeChatId, { content }, {
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
          if (name === "ask_human_question" || name === "request_human_takeover") return;
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
    [activeChatId, updateChatTitle, updateAgent],
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
    ChatContext.Provider,
    {
      value: {
        activeChatId,
        activeChat,
        messages,
        sending,
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

export function useChat(): ChatContextValue {
  const ctx = useContext(ChatContext);
  if (!ctx) throw new Error("useChat must be used within ChatProvider");
  return ctx;
}
