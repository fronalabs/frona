"use client";

import { useRouter } from "next/navigation";
import {
  ArrowPathIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  StopCircleIcon,
} from "@heroicons/react/24/outline";
import { formatDistanceToNowStrict } from "date-fns";
import { cancelExecution, executionHref } from "@/lib/activity-actions";
import { useActivity } from "@/lib/activity-context";
import type { Execution } from "@/lib/types";

export function executionsForChat(
  executions: Execution[],
  chatId: string,
): Execution[] {
  return executions.filter(
    (execution) =>
      execution.kind !== "inference" &&
      execution.relatedChatIds?.includes(chatId),
  );
}

export function useChatExecutions(chatId?: string): Execution[] {
  const { snapshot } = useActivity();
  if (!chatId) return [];
  return executionsForChat(snapshot.executions, chatId);
}

export function ChatActivityTab({
  executions,
  onExpand,
}: {
  executions: Execution[];
  onExpand: () => void;
}) {
  if (executions.length === 0) return null;

  return (
    <button
      type="button"
      onClick={onExpand}
      className="mb-[-1px] flex max-w-[80%] items-center gap-2 rounded-t-xl border border-b-0 border-border bg-surface-secondary px-4 py-1.5 text-text-secondary transition hover:bg-surface-tertiary"
      aria-label={`Expand ${executions.length} background ${executions.length === 1 ? "activity" : "activities"}`}
    >
      <ArrowPathIcon className="h-4 w-4 shrink-0 animate-spin text-info-text" />
      <span className="min-w-0 truncate text-left text-xs font-medium">
        {executions[0].title}
      </span>
      {executions.length > 1 && (
        <span className="shrink-0 rounded-full bg-surface-tertiary px-2 py-0.5 text-[10px] font-medium text-text-tertiary">
          +{executions.length - 1}
        </span>
      )}
      <ChevronUpIcon className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
    </button>
  );
}

export function ChatActivityDrawer({
  executions,
  onCollapse,
}: {
  executions: Execution[];
  onCollapse: () => void;
}) {
  const { refresh } = useActivity();
  const router = useRouter();

  if (executions.length === 0) return null;

  const openExecution = (execution: Execution) => {
    const href = executionHref(execution);
    if (href) router.push(href);
  };

  const handleCancel = async (execution: Execution) => {
    try {
      await cancelExecution(execution);
      await refresh();
    } catch {
      // The next SSE event or fallback poll will reconcile the snapshot.
    }
  };

  return (
    <div className="tool-drawer group/activity relative px-4 pb-2 pt-3">
      <div className="absolute inset-x-0 -top-4 h-5" />
      <button
        type="button"
        onClick={onCollapse}
        className="absolute left-1/2 top-[-0.75rem] z-10 flex h-6 w-6 -translate-x-1/2 items-center justify-center rounded-full border border-border bg-surface text-text-tertiary opacity-0 shadow-sm transition hover:bg-surface-secondary hover:text-text-primary group-hover/activity:opacity-100 focus:opacity-100"
        title="Collapse activity"
        aria-label="Collapse background activity"
      >
        <ChevronDownIcon className="h-3 w-3" />
      </button>

      <div className="mb-1 flex items-center gap-2">
        <ArrowPathIcon className="h-5 w-5 animate-spin text-info-text" />
        <span className="text-xs font-medium uppercase tracking-wide text-text-secondary">
          Background activity
        </span>
        <span className="rounded-full bg-surface-tertiary px-2 py-0.5 text-[10px] font-medium text-text-tertiary">
          {executions.length}
        </span>
      </div>

      <div className="max-h-56 overflow-y-auto">
        {executions.map((execution) => {
          const href = executionHref(execution);
          return (
            <div
              key={execution.id}
              className="group flex items-start gap-3 rounded-lg px-2 py-2 hover:bg-surface-tertiary"
            >
              <span className="mt-1.5 h-2 w-2 shrink-0 animate-pulse rounded-full bg-info-text" />
              <button
                type="button"
                onClick={() => openExecution(execution)}
                disabled={!href}
                className="min-w-0 flex-1 text-left disabled:cursor-default"
              >
                <span className="flex min-w-0 items-baseline gap-2">
                  <span className="min-w-0 truncate text-sm font-medium text-text-primary">
                    {execution.title}
                  </span>
                  <span className="shrink-0 text-[11px] font-normal text-text-tertiary">
                    {formatDistanceToNowStrict(new Date(execution.startedAt))}
                  </span>
                </span>
                <span className="mt-0.5 block truncate text-xs text-text-secondary">
                  {execution.action ?? execution.status}
                </span>
              </button>
              {execution.canCancel && execution.source?.id && (
                <button
                  type="button"
                  onClick={() => void handleCancel(execution)}
                  className="mt-0.5 rounded-md p-1.5 text-text-tertiary transition hover:bg-danger-bg hover:text-danger-text"
                  title="Cancel activity"
                  aria-label={`Cancel ${execution.title}`}
                >
                  <StopCircleIcon className="h-5 w-5" />
                </button>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
