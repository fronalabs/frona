"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  createElement,
} from "react";
import { api, archiveChat as apiArchiveChat, unarchiveChat as apiUnarchiveChat, deleteChat as apiDeleteChat, deleteTask as apiDeleteTask, getArchivedChats, getContacts, getTask } from "./api-client";
import type {
  SpaceWithChats,
  ChatResponse,
  TaskResponse,
  Agent,
  Contact,
} from "./types";
import { indexContactsById } from "./types";

type ActiveTab = "chat" | "tasks" | "agents";

interface NavigationContextValue {
  spaces: SpaceWithChats[];
  standaloneChats: ChatResponse[];
  tasks: TaskResponse[];
  agents: Agent[];
  contacts: Record<string, Contact>;
  archivedChats: ChatResponse[];
  showArchived: boolean;
  setShowArchived: (show: boolean) => void;
  activeTab: ActiveTab;
  loading: boolean;
  setActiveTab: (tab: ActiveTab) => void;
  refresh: () => Promise<void>;
  addStandaloneChat: (chat: ChatResponse) => void;
  updateChatTitle: (chatId: string, title: string) => void;
  updateAgent: (agentId: string, fields: Record<string, unknown>) => void;
  archiveChat: (chatId: string) => Promise<void>;
  unarchiveChat: (chatId: string) => Promise<void>;
  deleteChat: (chatId: string) => Promise<void>;
  deleteTask: (taskId: string) => Promise<void>;
  updateTaskInList: (taskId: string, fields: Partial<TaskResponse>) => void;
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
  const [contacts, setContacts] = useState<Record<string, Contact>>({});
  const [archivedChats, setArchivedChats] = useState<ChatResponse[]>([]);
  const [showArchived, setShowArchived] = useState(false);
  const [activeTab, setActiveTab] = useState<ActiveTab>("chat");
  const [loading, setLoading] = useState(true);

  const refresh = useCallback(async () => {
    try {
      const [nav, tasksData, agentsData, contactsData] = await Promise.all([
        api.get<NavigationResponse>("/api/navigation"),
        api.get<TaskResponse[]>("/api/tasks"),
        api.get<Agent[]>("/api/agents"),
        getContacts(),
      ]);
      setSpaces(nav.spaces);
      setStandaloneChats(nav.standalone_chats);
      setTasks(tasksData);
      setAgents(agentsData);
      setContacts(indexContactsById(contactsData));
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

  const archiveChat = useCallback(async (chatId: string) => {
    await apiArchiveChat(chatId);
    setStandaloneChats((prev) => prev.filter((c) => c.id !== chatId));
    setSpaces((prev) =>
      prev.map((space) => ({
        ...space,
        chats: space.chats.filter((c) => c.id !== chatId),
      })),
    );
    const archived = await getArchivedChats();
    setArchivedChats(archived);
  }, []);

  const unarchiveChat = useCallback(async (chatId: string) => {
    await apiUnarchiveChat(chatId);
    setArchivedChats((prev) => prev.filter((c) => c.id !== chatId));
    await refresh();
  }, [refresh]);

  const deleteChatAction = useCallback(async (chatId: string) => {
    await apiDeleteChat(chatId);
    setStandaloneChats((prev) => prev.filter((c) => c.id !== chatId));
    setSpaces((prev) =>
      prev.map((space) => ({
        ...space,
        chats: space.chats.filter((c) => c.id !== chatId),
      })),
    );
    setArchivedChats((prev) => prev.filter((c) => c.id !== chatId));
  }, []);

  const deleteTaskAction = useCallback(async (taskId: string) => {
    const task = tasks.find((t) => t.id === taskId);
    await apiDeleteTask(taskId);
    setTasks((prev) => prev.filter((t) => t.id !== taskId));
    if (task?.chat_id) {
      const chatId = task.chat_id;
      setStandaloneChats((prev) => prev.filter((c) => c.id !== chatId));
      setSpaces((prev) =>
        prev.map((space) => ({
          ...space,
          chats: space.chats.filter((c) => c.id !== chatId),
        })),
      );
    }
  }, [tasks]);

  const updateTaskInList = useCallback((taskId: string, fields: Partial<TaskResponse>) => {
    setTasks((prev) => {
      const idx = prev.findIndex((t) => t.id === taskId);
      if (idx !== -1) {
        const updated = [...prev];
        updated[idx] = { ...updated[idx], ...fields };
        return updated;
      }
      return prev;
    });
    const status = fields.status ?? "pending";
    if (status === "pending" || status === "inprogress") {
      getTask(taskId)
        .then((task) => {
          setTasks((prev) => {
            if (prev.some((t) => t.id === task.id)) return prev;
            return [task, ...prev];
          });
        })
        .catch(() => {});
    }
  }, []);

  useEffect(() => {
    if (showArchived) {
      getArchivedChats().then(setArchivedChats).catch(() => {});
    }
  }, [showArchived]);

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
        contacts,
        archivedChats,
        showArchived,
        setShowArchived,
        activeTab,
        loading,
        setActiveTab,
        refresh,
        addStandaloneChat,
        updateChatTitle,
        updateAgent,
        archiveChat,
        unarchiveChat,
        deleteChat: deleteChatAction,
        deleteTask: deleteTaskAction,
        updateTaskInList,
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

export function neighborRoute(
  items: { id: string }[],
  deletedId: string,
  urlFn: (id: string) => string,
): string | null {
  const idx = items.findIndex((item) => item.id === deletedId);
  if (idx === -1) return null;
  const neighbor = items[idx + 1] ?? items[idx - 1];
  return neighbor ? urlFn(neighbor.id) : null;
}
