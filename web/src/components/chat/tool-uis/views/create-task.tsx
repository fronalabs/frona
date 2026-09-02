"use client";

import { ClockIcon, UserCircleIcon } from "@heroicons/react/24/outline";
import { cn } from "@/lib/utils";
import { ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

function formatTime(iso: string, timezone?: string): string {
  try {
    const d = new Date(iso);
    return new Intl.DateTimeFormat("en-US", {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      timeZone: timezone,
      timeZoneName: "short",
    }).format(d);
  } catch {
    return iso;
  }
}

interface ParsedResult {
  taskId?: string;
  targetAgent?: string;
  runAt?: string | null;
}

function parseResult(result: unknown): ParsedResult | null {
  let obj: unknown = null;
  if (typeof result === "string") {
    try {
      obj = JSON.parse(result);
    } catch {
      return null;
    }
  } else if (result && typeof result === "object") {
    obj = result;
  }
  if (!obj || typeof obj !== "object") return null;
  const o = obj as Record<string, unknown>;
  if (typeof o.task_id !== "string") return null;
  return {
    taskId: o.task_id,
    targetAgent: typeof o.target_agent === "string" ? o.target_agent : undefined,
    runAt: typeof o.run_at === "string" ? o.run_at : null,
  };
}

function ScheduledBlock({
  runAt,
  timezone,
}: {
  runAt: string;
  timezone?: string;
}) {
  return (
    <div className="rounded-lg bg-surface-nav p-4 flex items-center gap-2 text-[0.8125rem]">
      <ClockIcon className="h-4 w-4 shrink-0 text-text-tertiary" />
      <span className="font-medium text-text-primary">
        Scheduled for {formatTime(runAt, timezone)}
      </span>
    </div>
  );
}

export const CreateTaskView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const title = typeof a.title === "string" ? a.title : "";
  const instruction = typeof a.instruction === "string" ? a.instruction : "";
  const argsTimezone = typeof a.timezone === "string" ? a.timezone : undefined;
  const targetAgentArg = typeof a.target_agent === "string" ? a.target_agent : null;
  const processResult = a.process_result === true;
  const argsRunAt = typeof a.run_at === "string" ? a.run_at : null;
  const delayMinutes =
    typeof a.delay_minutes === "number" ? a.delay_minutes : null;

  const parsed = parseResult(result);
  if (result !== undefined && parsed === null) {
    return <ToolViewFallback />;
  }

  const runAt = parsed?.runAt ?? argsRunAt;
  const targetAgent = parsed?.targetAgent ?? targetAgentArg;

  const expandable = !!(instruction || runAt || delayMinutes != null);

  return (
    <ToolRow status={status} expandable={expandable}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Create Task</ToolRow.Title>
        <ToolRow.Subtitle>{title || null}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex flex-col gap-2">
          {runAt && <ScheduledBlock runAt={runAt} timezone={argsTimezone} />}
          <div
            className={cn(
              "flex flex-col gap-3 text-xs",
              runAt ? "px-3 pb-3" : "p-3",
            )}
          >
            {instruction && (
              <div className="flex flex-col gap-1">
                <p className="font-semibold text-text-secondary">Instruction</p>
                <p className="text-text-secondary whitespace-pre-wrap m-0">
                  {instruction}
                </p>
              </div>
            )}
            {(targetAgent || processResult) && (
              <div className="flex flex-wrap gap-x-4 gap-y-1 text-text-tertiary">
                {targetAgent && (
                  <span className="inline-flex items-center gap-1">
                    <UserCircleIcon className="h-3.5 w-3.5" />
                    <span className="text-text-secondary">{targetAgent}</span>
                  </span>
                )}
                {processResult && (
                  <span>
                    Resume on result{" "}
                    <span className="text-text-secondary">yes</span>
                  </span>
                )}
              </div>
            )}
          </div>
        </div>
      </ToolRow.Body>
    </ToolRow>
  );
};
