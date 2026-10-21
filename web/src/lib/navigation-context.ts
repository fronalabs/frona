"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  useRef,
  createElement,
} from "react";
import { api, archiveChat as apiArchiveChat, unarchiveChat as apiUnarchiveChat, deleteChat as apiDeleteChat, deleteAgent as apiDeleteAgent, deleteTask as apiDeleteTask, getArchivedChats, getContacts, getTask } from "./api-client";
import type {
  SpaceWithChats,
  ChatResponse,
  TaskResponse,
  Agent,
  Contact,
} from "./types";
import { indexContactsById } from "./types";

type ActiveTab = "chat" | "tasks";

/// Internal shape. Boot fields are `T | undefined` until `refresh()` lands;
/// `useNavigation` narrows them and throws if called before AppGate marks
/// the data ready, so consumers get the [[NavigationContextValue]] view.
interface NavigationContextRaw {
  spaces: SpaceWithChats[] | undefined;
  standaloneChats: ChatResponse[] | undefined;
  tasks: TaskResponse[] | undefined;
  agents: Agent[] | undefined;
  contacts: Record<string, Contact> | undefined;
  archivedChats: ChatResponse[];
  showArchived: boolean;
  setShowArchived: (show: boolean) => void;
  activeTab: ActiveTab;
  setActiveTab: (tab: ActiveTab) => void;
  mobileNavOpen: boolean;
  setMobileNavOpen: (open: boolean) => void;
  mobileSubNavOpen: boolean;
  setMobileSubNavOpen: (open: boolean) => void;
  /// Throws on network unavailability — AppGate decides whether to retry.
  refresh: () => Promise<void>;
  addStandaloneChat: (chat: ChatResponse) => void;
  updateChatTitle: (chatId: string, title: string) => void;
  updateAgent: (agentId: string, fields: Record<string, unknown>) => void;
  deleteAgent: (agentId: string) => Promise<void>;
  archiveChat: (chatId: string) => Promise<void>;
  unarchiveChat: (chatId: string) => Promise<void>;
  deleteChat: (chatId: string) => Promise<void>;
  deleteTask: (taskId: string) => Promise<void>;
  updateTaskInList: (taskId: string, fields: Partial<TaskResponse>) => void;
}

export type NavigationContextValue = Omit<
  NavigationContextRaw,
  "spaces" | "standaloneChats" | "tasks" | "agents" | "contacts"
> & {
  spaces: SpaceWithChats[];
  standaloneChats: ChatResponse[];
  tasks: TaskResponse[];
  agents: Agent[];
  contacts: Record<string, Contact>;
};

const NavigationContext = createContext<NavigationContextRaw | null>(null);

interface NavigationResponse {
  spaces: SpaceWithChats[];
  standalone_chats: ChatResponse[];
}

export function NavigationProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const sortAgents = (list: Agent[]) =>
    [...list].sort((a, b) =>
      a.handle === "system" ? -1 : b.handle === "system" ? 1 : a.name.localeCompare(b.name),
    );

  const [spaces, setSpaces] = useState<SpaceWithChats[] | undefined>(undefined);
  const [standaloneChats, setStandaloneChats] = useState<ChatResponse[] | undefined>(undefined);
  const [tasks, setTasks] = useState<TaskResponse[] | undefined>(undefined);
  const taskRequests = useRef(new Map<string, symbol>());
  const [agents, setAgents] = useState<Agent[] | undefined>(undefined);
  const [contacts, setContacts] = useState<Record<string, Contact> | undefined>(undefined);
  const [archivedChats, setArchivedChats] = useState<ChatResponse[]>([]);
  const [showArchived, setShowArchived] = useState(false);
  const [activeTab, setActiveTab] = useState<ActiveTab>("chat");
  const [mobileNavOpen, setMobileNavOpen] = useState(false);
  const [mobileSubNavOpen, setMobileSubNavOpen] = useState(false);

  /// State is set atomically on success — UI sees the previous snapshot
  /// until the new one lands.
  const refresh = useCallback(async () => {
    const [nav, tasksData, agentsData, contactsData] = await Promise.all([
      api.get<NavigationResponse>("/api/navigation"),
      api.get<TaskResponse[]>("/api/tasks"),
      api.get<Agent[]>("/api/agents"),
      getContacts(),
    ]);
    setSpaces(nav.spaces);
    setStandaloneChats(nav.standalone_chats);
    setTasks(tasksData);
    setAgents(sortAgents(agentsData));
    setContacts(indexContactsById(contactsData));
  }, []);

  // `prev ? ... : prev` guards exist only to satisfy the type system; the
  // mutators are only reachable below AppGate, which guarantees boot data.
  const addStandaloneChat = useCallback((chat: ChatResponse) => {
    setStandaloneChats((prev) => (prev ? [chat, ...prev] : prev));
  }, []);

  const updateAgent = useCallback((agentId: string, fields: Record<string, unknown>) => {
    setAgents((prev) => {
      if (!prev) return prev;
      const exists = prev.some((a) => a.id === agentId);
      if (exists) {
        return prev.map((a) => (a.id === agentId ? { ...a, ...fields } : a));
      }
      api.get<Agent>(`/api/agents/${agentId}`).then((agent) => {
        setAgents((curr) => {
          if (!curr) return curr;
          if (curr.some((a) => a.id === agentId)) return curr;
          return sortAgents([...curr, agent]);
        });
      }).catch(() => {});
      return prev;
    });
  }, []);

  const deleteAgentAction = useCallback(async (agentId: string) => {
    await apiDeleteAgent(agentId);
    setAgents((prev) => prev?.filter((a) => a.id !== agentId));
  }, []);

  const archiveChat = useCallback(async (chatId: string) => {
    await apiArchiveChat(chatId);
    setStandaloneChats((prev) => prev?.filter((c) => c.id !== chatId));
    setSpaces((prev) =>
      prev?.map((space) => ({
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
    setStandaloneChats((prev) => prev?.filter((c) => c.id !== chatId));
    setSpaces((prev) =>
      prev?.map((space) => ({
        ...space,
        chats: space.chats.filter((c) => c.id !== chatId),
      })),
    );
    setArchivedChats((prev) => prev.filter((c) => c.id !== chatId));
  }, []);

  const deleteTaskAction = useCallback(async (taskId: string) => {
    const task = tasks?.find((t) => t.id === taskId);
    await apiDeleteTask(taskId);
    taskRequests.current.delete(taskId);
    setTasks((prev) => prev?.filter((t) => t.id !== taskId));
    if (task?.chat_id) {
      const chatId = task.chat_id;
      setStandaloneChats((prev) => prev?.filter((c) => c.id !== chatId));
      setSpaces((prev) =>
        prev?.map((space) => ({
          ...space,
          chats: space.chats.filter((c) => c.id !== chatId),
        })),
      );
    }
  }, [tasks]);

  const updateTaskInList = useCallback((taskId: string, fields: Partial<TaskResponse>) => {
    const request = Symbol();
    taskRequests.current.set(taskId, request);
    setTasks((prev) => {
      if (!prev) return prev;
      const idx = prev.findIndex((t) => t.id === taskId);
      if (idx !== -1) {
        const updated = [...prev];
        updated[idx] = { ...updated[idx], ...fields };
        return updated;
      }
      return prev;
    });
    // Live events omit run dates and updated_at. Refresh those keys without
    // moving existing rows, and ignore responses superseded by another event.
    getTask(taskId)
      .then((task) => {
        setTasks((prev) => {
          if (!prev || taskRequests.current.get(taskId) !== request) return prev;
          if (prev.some(t => t.id === task.id)) {
            return prev.map(t => t.id === task.id ? task : t);
          }
          return [task, ...prev];
        });
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (showArchived) {
      getArchivedChats().then(setArchivedChats).catch(() => {});
    }
  }, [showArchived]);

  const updateChatTitle = useCallback((chatId: string, title: string) => {
    setStandaloneChats((prev) =>
      prev?.map((c) => (c.id === chatId ? { ...c, title } : c)),
    );
    setSpaces((prev) =>
      prev?.map((space) => ({
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
        setActiveTab,
        mobileNavOpen,
        setMobileNavOpen,
        mobileSubNavOpen,
        setMobileSubNavOpen,
        refresh,
        addStandaloneChat,
        updateChatTitle,
        updateAgent,
        deleteAgent: deleteAgentAction,
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
  if (
    ctx.spaces === undefined ||
    ctx.standaloneChats === undefined ||
    ctx.tasks === undefined ||
    ctx.agents === undefined ||
    ctx.contacts === undefined
  ) {
    throw new Error(
      "useNavigation called before AppGate finished loading. Components below AppGate can rely on boot data being present; if you need the loose shape (e.g. inside AppGate itself), use useNavigationRaw.",
    );
  }
  return ctx as NavigationContextValue;
}

/// Only AppGate needs this — it gates rendering on whether data has loaded.
export function useNavigationRaw(): NavigationContextRaw {
  const ctx = useContext(NavigationContext);
  if (!ctx)
    throw new Error("useNavigationRaw must be used within NavigationProvider");
  return ctx;
}

export function useSystemAgent(): Agent {
  const { agents } = useNavigation();
  const found = agents.find((a) => a.handle === "system");
  if (!found) {
    throw new Error(
      "Invariant violated: 'system' agent missing. Every user is supposed to have one cloned at signup.",
    );
  }
  return found;
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
