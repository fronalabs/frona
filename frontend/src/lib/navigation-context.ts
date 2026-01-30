"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  createElement,
} from "react";
import { api } from "./api-client";
import type {
  SpaceWithChats,
  ChatResponse,
  TaskResponse,
  Agent,
} from "./types";

type ActiveTab = "chat" | "tasks" | "agents";

interface NavigationContextValue {
  spaces: SpaceWithChats[];
  standaloneChats: ChatResponse[];
  tasks: TaskResponse[];
  agents: Agent[];
  activeTab: ActiveTab;
  loading: boolean;
  setActiveTab: (tab: ActiveTab) => void;
  refresh: () => Promise<void>;
  addStandaloneChat: (chat: ChatResponse) => void;
  updateChatTitle: (chatId: string, title: string) => void;
  updateAgent: (agentId: string, fields: Record<string, unknown>) => void;
}

const NavigationContext = createContext<NavigationContextValue | null>(null);

interface NavigationResponse {
  spaces: SpaceWithChats[];
  standalone_chats: ChatResponse[];
}

export function NavigationProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [spaces, setSpaces] = useState<SpaceWithChats[]>([]);
  const [standaloneChats, setStandaloneChats] = useState<ChatResponse[]>([]);
  const [tasks, setTasks] = useState<TaskResponse[]>([]);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [activeTab, setActiveTab] = useState<ActiveTab>("chat");
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      const [nav, tasksData, agentsData] = await Promise.all([
        api.get<NavigationResponse>("/api/navigation"),
        api.get<TaskResponse[]>("/api/tasks"),
        api.get<Agent[]>("/api/agents"),
      ]);
      setSpaces(nav.spaces);
      setStandaloneChats(nav.standalone_chats);
      setTasks(tasksData);
      setAgents(agentsData);
    } catch {
      // silently fail - auth guard will redirect if needed
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const addStandaloneChat = useCallback((chat: ChatResponse) => {
    setStandaloneChats((prev) => [chat, ...prev]);
  }, []);

  const updateAgent = useCallback((agentId: string, fields: Record<string, unknown>) => {
    setAgents((prev) =>
      prev.map((a) => (a.id === agentId ? { ...a, ...fields } : a)),
    );
  }, []);

  const updateChatTitle = useCallback((chatId: string, title: string) => {
    setStandaloneChats((prev) =>
      prev.map((c) => (c.id === chatId ? { ...c, title } : c)),
    );
    setSpaces((prev) =>
      prev.map((space) => ({
        ...space,
        chats: space.chats.map((c) =>
          c.id === chatId ? { ...c, title } : c,
        ),
      })),
    );
  }, []);

  return createElement(
    NavigationContext.Provider,
    {
      value: {
        spaces,
        standaloneChats,
        tasks,
        agents,
        activeTab,
        loading,
        setActiveTab,
        refresh,
        addStandaloneChat,
        updateChatTitle,
        updateAgent,
      },
    },
    children,
  );
}

export function useNavigation(): NavigationContextValue {
  const ctx = useContext(NavigationContext);
  if (!ctx)
    throw new Error("useNavigation must be used within NavigationProvider");
  return ctx;
}
