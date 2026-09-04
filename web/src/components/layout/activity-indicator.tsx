"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import {
  ArrowPathIcon,
  StopCircleIcon,
  UserCircleIcon,
} from "@heroicons/react/24/outline";
import { formatDistanceToNowStrict } from "date-fns";
import { useActivity } from "@/lib/activity-context";
import { cancelExecution, executionHref } from "@/lib/activity-actions";
import {
  executionKindLabel,
  executionMetadataLabel,
} from "@/lib/activity-labels";
import type { Execution } from "@/lib/types";

export function ActivityIndicator({ compact = false }: { compact?: boolean }) {
  const { snapshot, stale, refresh } = useActivity();
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const router = useRouter();
  const executions = snapshot.executions;

  useEffect(() => {
    if (!open) return;
    const handlePointerDown = (event: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    return () => document.removeEventListener("mousedown", handlePointerDown);
  }, [open]);

  if (executions.length === 0) return null;

  const openExecution = (execution: Execution) => {
    const href = executionHref(execution);
    if (!href) return;
    setOpen(false);
    router.push(href);
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
    <div ref={rootRef} className="relative flex items-center">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        className={`relative flex items-center justify-center gap-1.5 border transition ${
          open
            ? "z-[61] rounded-t-xl rounded-b-none border-border border-b-0 bg-surface-secondary text-text-primary"
            : "rounded-full border-info-text/25 bg-info-bg text-info-text hover:border-info-text/40"
        } ${
          compact ? "h-10 min-w-10 px-2" : "h-10 px-3"
        }`}
        aria-expanded={open}
        aria-label={`${executions.length} active ${executions.length === 1 ? "execution" : "executions"}`}
      >
        <ArrowPathIcon className="h-4 w-4 animate-spin" />
        {compact ? (
          <span className="text-xs font-semibold tabular-nums">{executions.length}</span>
        ) : (
          <span className="text-sm font-medium whitespace-nowrap">
            {executions.length} active
          </span>
        )}
      </button>

      {open && (
        <>
          <div className="absolute right-0 top-full z-[60] w-[min(24rem,calc(100vw-1.5rem))] overflow-hidden rounded-xl rounded-tr-none border border-border bg-surface-secondary shadow-lg">
            <div className="flex shrink-0 items-center justify-between border-b border-border px-4 py-2">
              <div>
                <h2 className="text-sm font-medium text-text-secondary">Activity</h2>
                {stale && (
                  <p className="text-xs text-text-tertiary">Reconnecting to the server</p>
                )}
              </div>
            </div>

            <div className="max-h-96 overflow-y-auto pb-1">
              {executions.map((execution) => {
                const href = executionHref(execution);
                return (
                  <div
                    key={execution.id}
                    className="group flex items-start gap-3 px-4 py-2 transition hover:bg-surface-tertiary"
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
                        {execution.action ?? executionKindLabel(execution)}
                      </span>
                      <span className="mt-1 flex items-center gap-1 text-[11px] text-text-tertiary">
                        <UserCircleIcon className="h-3.5 w-3.5 shrink-0" />
                        {executionMetadataLabel(execution)}
                      </span>
                    </button>
                    {execution.canCancel && execution.source?.id && (
                      <button
                        type="button"
                        onClick={() => void handleCancel(execution)}
                        className="mt-0.5 rounded-md p-1.5 text-text-tertiary hover:bg-danger-bg hover:text-danger-text transition"
                        title="Cancel execution"
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
          <div
            aria-hidden="true"
            className="absolute right-0 top-[calc(100%-1px)] z-[60] h-[2px] w-full bg-surface-secondary"
          />
        </>
      )}
    </div>
  );
}
