"use client";

import {
  createContext,
  createElement,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { getActivity } from "./api-client";
import { useNavigation } from "./navigation-context";
import { sseBus, type ChatSSEEvent } from "./sse-event-bus";
import type { ActivitySnapshot, Execution, TaskResponse } from "./types";

const ACTIVE_POLL_INTERVAL_MS = 20_000;
const EMPTY_SNAPSHOT: ActivitySnapshot = { executions: [] };

function executionSourceKey(execution: Execution): string | null {
  const source = execution.source;
  if (!source?.id || (source.type !== "task" && source.type !== "schedule")) {
    return null;
  }
  return `${source.type}:${source.id}`;
}

function taskSourceKey(task: TaskResponse): string {
  if (task.kind.type === "CronRun") {
    return `schedule:${task.kind.source_cron_id}`;
  }
  return `task:${task.id}`;
}

function toolLabel(name: string): string {
  return name
    .split("__")
    .at(-1)!
    .split("_")
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

function actionFromChatEvent(event: ChatSSEEvent): string | null {
  if (event.type === "tool_call") {
    return event.description?.trim() || `Running ${toolLabel(event.name)}`;
  }
  if (event.type === "tool_result") {
    const label = toolLabel(event.name);
    return event.success ? `Reviewing ${label} result` : `${label} failed`;
  }
  return null;
}

export function applyLiveTaskActions(
  snapshot: ActivitySnapshot,
  tasks: TaskResponse[],
  actionsByChatId: ReadonlyMap<string, string>,
): ActivitySnapshot {
  if (actionsByChatId.size === 0 || snapshot.executions.length === 0) {
    return snapshot;
  }

  const executionCountBySource = new Map<string, number>();
  for (const execution of snapshot.executions) {
    const key = executionSourceKey(execution);
    if (key) executionCountBySource.set(key, (executionCountBySource.get(key) ?? 0) + 1);
  }

  const actionBySource = new Map<string, string>();
  const ambiguousSources = new Set<string>();
  for (const task of tasks) {
    if (!task.chat_id) continue;
    const action = actionsByChatId.get(task.chat_id);
    if (!action) continue;
    const key = taskSourceKey(task);
    if (actionBySource.has(key)) {
      ambiguousSources.add(key);
    } else {
      actionBySource.set(key, action);
    }
  }

  let changed = false;
  const executions = snapshot.executions.map((execution) => {
    const key = executionSourceKey(execution);
    if (
      !key ||
      executionCountBySource.get(key) !== 1 ||
      ambiguousSources.has(key)
    ) {
      return execution;
    }
    const action = actionBySource.get(key);
    if (!action || action === execution.action) return execution;
    changed = true;
    return { ...execution, action };
  });

  return changed ? { ...snapshot, executions } : snapshot;
}

interface ActivityContextValue {
  snapshot: ActivitySnapshot;
  loading: boolean;
  stale: boolean;
  refresh: () => Promise<void>;
}

const ActivityContext = createContext<ActivityContextValue | null>(null);

export function ActivityProvider({ children }: { children: React.ReactNode }) {
  const { tasks } = useNavigation();
  const [snapshot, setSnapshot] = useState<ActivitySnapshot>(EMPTY_SNAPSHOT);
  const [liveActionsByChatId, setLiveActionsByChatId] = useState<Map<string, string>>(
    () => new Map(),
  );
  const [loading, setLoading] = useState(true);
  const [stale, setStale] = useState(false);
  const snapshotRef = useRef(snapshot);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const inFlightRef = useRef<Promise<void> | null>(null);
  const refreshQueuedRef = useRef(false);
  const mountedRef = useRef(true);
  const refreshRef = useRef<() => Promise<void>>(async () => {});

  const clearPoll = useCallback(() => {
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current);
      timeoutRef.current = null;
    }
  }, []);

  const schedulePoll = useCallback((force = false) => {
    clearPoll();
    if (!force && snapshotRef.current.executions.length === 0) return;
    timeoutRef.current = setTimeout(() => {
      timeoutRef.current = null;
      void refreshRef.current();
    }, ACTIVE_POLL_INTERVAL_MS);
  }, [clearPoll]);

  const refresh = useCallback(async () => {
    clearPoll();
    if (inFlightRef.current) {
      refreshQueuedRef.current = true;
      return inFlightRef.current;
    }

    let failed = false;
    const request = getActivity()
      .then((next) => {
        if (!mountedRef.current) return;
        if (next.executions.length === 0) setLiveActionsByChatId(new Map());
        snapshotRef.current = next;
        setSnapshot(next);
        setStale(false);
      })
      .catch(() => {
        failed = true;
        if (mountedRef.current) setStale(true);
      })
      .finally(() => {
        inFlightRef.current = null;
        if (!mountedRef.current) return;
        setLoading(false);
        if (refreshQueuedRef.current) {
          refreshQueuedRef.current = false;
          void refreshRef.current();
        } else {
          schedulePoll(failed);
        }
      });

    inFlightRef.current = request;
    return request;
  }, [clearPoll, schedulePoll]);

  refreshRef.current = refresh;

  useEffect(() => {
    mountedRef.current = true;
    const unsubscribeGlobal = sseBus.onGlobal((event) => {
      if (event.type === "activity_changed") void refreshRef.current();
    });
    const unsubscribeChatEvents = sseBus.onChatEvent((chatId, event) => {
      const action = actionFromChatEvent(event);
      if (action) {
        setLiveActionsByChatId((current) => {
          if (current.get(chatId) === action) return current;
          const next = new Map(current);
          next.set(chatId, action);
          return next;
        });
        return;
      }
      if (
        event.type === "inference_done" ||
        event.type === "inference_cancelled" ||
        event.type === "inference_error"
      ) {
        setLiveActionsByChatId((current) => {
          if (!current.has(chatId)) return current;
          const next = new Map(current);
          next.delete(chatId);
          return next;
        });
      }
    });
    const unsubscribeReconnect = sseBus.onReconnect(() => {
      void refreshRef.current();
    });
    const handleVisibility = () => {
      if (document.visibilityState === "visible") void refreshRef.current();
    };
    document.addEventListener("visibilitychange", handleVisibility);
    void refreshRef.current();

    return () => {
      mountedRef.current = false;
      clearPoll();
      unsubscribeGlobal();
      unsubscribeChatEvents();
      unsubscribeReconnect();
      document.removeEventListener("visibilitychange", handleVisibility);
    };
  }, [clearPoll]);

  const displaySnapshot = useMemo(
    () => applyLiveTaskActions(snapshot, tasks, liveActionsByChatId),
    [snapshot, tasks, liveActionsByChatId],
  );

  return createElement(
    ActivityContext.Provider,
    { value: { snapshot: displaySnapshot, loading, stale, refresh } },
    children,
  );
}

export function useActivity(): ActivityContextValue {
  const context = useContext(ActivityContext);
  if (!context) throw new Error("useActivity must be used within ActivityProvider");
  return context;
}
