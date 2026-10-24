"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  useMemo,
  useRef,
} from "react";
import { useAuiState, type ToolCallMessagePart, type ToolCallMessagePartStatus } from "@assistant-ui/react";
import { browserCallStatus, isBrowserTool } from "./browser-tool-state";
import { useThreadIsRunning } from "@assistant-ui/core/react";
import { ChevronRightIcon, ChevronUpIcon, WrenchScrewdriverIcon } from "@heroicons/react/24/outline";
import { AnimatePresence, motion } from "motion/react";

const MAX_VISIBLE = 6;
const COLLAPSE_DELAY_MS = 10_000;

/** Tool calls with custom UIs that render outside the timeline. */
const EXCLUDED_TOOLS = new Set([
  "Question",
  "Takeover",
  "Credential",
  "App",
  "Completion",
  "Deferred",
  "Attachments",
]);

interface ToolTimelineContextValue {
  browserCalls: readonly ToolCallMessagePart[];
  browserStatus?: ToolCallMessagePartStatus;
  isVisible: (toolCallId: string) => boolean;
  isLastVisible: (toolCallId: string) => boolean;
  isFirstVisible: (toolCallId: string) => boolean;
  getToolIndex: (toolCallId: string) => number;
  hiddenCount: number;
}

const Ctx = createContext<ToolTimelineContextValue | null>(null);

export function useToolTimeline() {
  return useContext(Ctx);
}

export function ToolTimelineProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const message = useAuiState((state) => state.message);
  const isRunning = useThreadIsRunning();

  const browserCalls = useMemo(() => message.content.filter(
    (part): part is ToolCallMessagePart => part.type === "tool-call" && isBrowserTool(part.toolName),
  ), [message.content]);
  const latestBrowserCall = browserCalls[browserCalls.length - 1];
  const browserStatus = latestBrowserCall ? browserCallStatus(latestBrowserCall, message.status) : undefined;

  const toolCallIds = useMemo(() => {
    if (message.role !== "assistant") return [];
    return (
      message.content as unknown as ReadonlyArray<{
        type: string;
        toolCallId?: string;
        toolName?: string;
      }>
    )
      .filter((p) => p.type === "tool-call" && p.toolCallId && !EXCLUDED_TOOLS.has(p.toolName ?? "")
        && (!isBrowserTool(p.toolName ?? "") || p.toolCallId === browserCalls[0]?.toolCallId))
      .map((p) => p.toolCallId!);
  }, [message.role, message.content, browserCalls]);

  // Stabilise the ID list so downstream memos only recompute when the
  // actual set of tool-call IDs changes, not on every content reference change.
  const toolCallKey = toolCallIds.join(",");
  const stableIdsRef = useRef<string[]>([]);
  const stableKeyRef = useRef<string>("");
  if (stableKeyRef.current !== toolCallKey) {
    stableIdsRef.current = toolCallIds;
    stableKeyRef.current = toolCallKey;
  }
  const stableToolCallIds = stableIdsRef.current;

  const totalTools = stableToolCallIds.length + Math.max(0, browserCalls.length - 1);

  // Messages loaded from history start collapsed; live messages start expanded.
  // Recently completed messages (remounted after streaming→real ID swap) start
  // expanded so the collapse animation can play out.
  const [collapsed, setCollapsed] = useState(() => {
    if (message.isLast && isRunning) return false;
    if (message.isLast && !isRunning && totalTools > 0) {
      const ts = (message as unknown as { createdAt?: Date }).createdAt;
      if (ts && Date.now() - new Date(ts).getTime() < COLLAPSE_DELAY_MS) {
        return false;
      }
    }
    return true;
  });
  const [userToggled, setUserToggled] = useState(false);

  const visibleSet = useMemo(() => {
    if (userToggled) return null; // user toggled — show all
    if (stableToolCallIds.length <= MAX_VISIBLE) return null; // fits — show all
    const set = new Set<string>();
    const start = Math.max(0, stableToolCallIds.length - MAX_VISIBLE);
    for (let i = start; i < stableToolCallIds.length; i++) {
      set.add(stableToolCallIds[i]);
    }
    const browserId = browserCalls[0]?.toolCallId;
    if (browserId && !set.has(browserId)) {
      set.delete(stableToolCallIds[start]);
      set.add(browserId);
    }
    return set;
  }, [stableToolCallIds, userToggled, browserCalls]);

  const messageIsLast = message.isLast;
  const messageCreatedAt = (message as unknown as { createdAt?: Date }).createdAt;
  useEffect(() => {
    if (isRunning && messageIsLast) {
      if (!userToggled && collapsed) {
        setCollapsed(false);
      }
      return;
    }
    if (!collapsed && !isRunning && messageIsLast && totalTools > 0 && !userToggled) {
      const age = messageCreatedAt ? Date.now() - new Date(messageCreatedAt).getTime() : COLLAPSE_DELAY_MS;
      const remaining = Math.max(100, COLLAPSE_DELAY_MS - age);
      const timer = setTimeout(() => setCollapsed(true), remaining);
      return () => clearTimeout(timer);
    }
  }, [isRunning, messageIsLast, messageCreatedAt, totalTools, userToggled, collapsed]);

  const isVisible = useCallback(
    (toolCallId: string) => {
      if (browserCalls.some((call) => call.toolCallId === toolCallId)) {
        return toolCallId === browserCalls[0]?.toolCallId && !collapsed;
      }
      if (collapsed) return false;
      if (visibleSet) return visibleSet.has(toolCallId);
      return true;
    },
    [collapsed, visibleSet, browserCalls],
  );

  const isLastVisible = useCallback(
    (toolCallId: string) => {
      return toolCallId === stableToolCallIds[stableToolCallIds.length - 1];
    },
    [stableToolCallIds],
  );

  const firstVisibleId = useMemo(() => {
    if (!visibleSet) return stableToolCallIds[0] ?? null;
    for (const id of stableToolCallIds) {
      if (visibleSet.has(id)) return id;
    }
    return null;
  }, [stableToolCallIds, visibleSet]);

  const isFirstVisible = useCallback(
    (toolCallId: string) => toolCallId === firstVisibleId,
    [firstVisibleId],
  );

  const toolIndexMap = useMemo(() => {
    const map = new Map<string, number>();
    stableToolCallIds.forEach((id, i) => map.set(id, i + 1));
    return map;
  }, [stableToolCallIds]);

  const getToolIndex = useCallback(
    (toolCallId: string) => toolIndexMap.get(toolCallId) ?? 0,
    [toolIndexMap],
  );

  const hiddenCount = visibleSet ? stableToolCallIds.length - visibleSet.size : 0;

  const handleToggle = useCallback(() => {
    setCollapsed((prev) => !prev);
    setUserToggled(true);
  }, []);

  const value = useMemo(
    () => ({ isVisible, isLastVisible, isFirstVisible, getToolIndex, hiddenCount, browserCalls, browserStatus }),
    [isVisible, isLastVisible, isFirstVisible, getToolIndex, hiddenCount, browserCalls, browserStatus],
  );

  if (totalTools === 0) return <>{children}</>;

  return (
    <Ctx.Provider value={value}>
      {children}
      <AnimatePresence mode="wait">
        {collapsed && (
          <motion.button
            key="expand"
            initial={{ opacity: 0, y: -4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -4 }}
            transition={{ duration: 0.2 }}
            onClick={handleToggle}
            className="inline-flex items-center gap-1.5 rounded-md py-1 text-xs text-text-tertiary hover:text-text-secondary hover:bg-surface-secondary transition-colors mt-2"
          >
            <WrenchScrewdriverIcon className="h-3 w-3" />
            <span>
              Used {totalTools} tool{totalTools !== 1 ? "s" : ""}
            </span>
            <ChevronRightIcon className="h-3 w-3" />
          </motion.button>
        )}
        {!collapsed && (!isRunning || !message.isLast) && totalTools > 0 && (
          <motion.button
            key="collapse"
            initial={{ opacity: 0, y: -4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -4 }}
            transition={{ duration: 0.2 }}
            onClick={handleToggle}
            className="inline-flex items-center gap-1.5 rounded-md py-1 text-xs text-text-tertiary hover:text-text-secondary hover:bg-surface-secondary transition-colors mt-1"
          >
            <WrenchScrewdriverIcon className="h-3 w-3" />
            <span>
              Hide {totalTools} tool{totalTools !== 1 ? "s" : ""}
            </span>
            <ChevronUpIcon className="h-3 w-3" />
          </motion.button>
        )}
      </AnimatePresence>
    </Ctx.Provider>
  );
}
