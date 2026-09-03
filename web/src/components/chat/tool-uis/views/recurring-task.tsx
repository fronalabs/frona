"use client";

import cronstrue from "cronstrue";
import { ClockIcon, UserCircleIcon } from "@heroicons/react/24/outline";
import { shouldUseToolViewFallback, ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

/**
 * Humanize a cron expression via `cronstrue`. Returns null when the
 * expression can't be parsed so the renderer falls back to the raw text.
 */
export function humanizeCron(expr: string): string | null {
  try {
    return cronstrue.toString(expr);
  } catch {
    return null;
  }
}

function formatNextRun(iso: string, timezone?: string): string {
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
  cronExpression?: string;
  timezone?: string;
  nextRunAt?: string;
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
  if (typeof o.task_id !== "string" || typeof o.cron_expression !== "string") {
    return null;
  }
  return {
    cronExpression: o.cron_expression,
    timezone: typeof o.timezone === "string" ? o.timezone : undefined,
    nextRunAt: typeof o.next_run_at === "string" ? o.next_run_at : undefined,
  };
}

function ScheduleBlock({
  cronExpression,
  nextRunAt,
  timezone,
}: {
  cronExpression: string;
  nextRunAt?: string;
  timezone?: string;
}) {
  const human = humanizeCron(cronExpression);
  const nextLocal = nextRunAt ? formatNextRun(nextRunAt, timezone) : null;
  return (
    <div className="rounded-lg bg-surface-nav p-4 flex flex-col gap-2 text-[0.8125rem]">
      <div className="flex items-center gap-2">
        <ClockIcon className="h-4 w-4 shrink-0 text-text-tertiary" />
        <span className="font-medium text-text-primary">{human ?? cronExpression}</span>
        {human && (
          <span className="font-mono text-xs text-text-tertiary">({cronExpression})</span>
        )}
      </div>
      {nextLocal && (
        <div className="text-xs text-text-tertiary pl-6">
          Next run: <span className="text-text-secondary">{nextLocal}</span>
        </div>
      )}
    </div>
  );
}

export const RecurringTaskView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const title = typeof a.title === "string" ? a.title : "";
  const instruction = typeof a.instruction === "string" ? a.instruction : "";
  const argsCron = typeof a.cron_expression === "string" ? a.cron_expression : "";
  const argsTimezone = typeof a.timezone === "string" ? a.timezone : undefined;
  const mode = typeof a.cron_mode === "string" ? a.cron_mode : "singleton";
  const concurrency = typeof a.cron_concurrency === "string" ? a.cron_concurrency : null;
  const targetAgent = typeof a.target_agent === "string" ? a.target_agent : null;

  const parsedResult = parseResult(result);
  if (shouldUseToolViewFallback(result, parsedResult !== null, status)) {
    return <ToolViewFallback />;
  }

  const cronExpression = parsedResult?.cronExpression || argsCron;
  const timezone = parsedResult?.timezone || argsTimezone;
  const nextRunAt = parsedResult?.nextRunAt;

  const expandable = !!(cronExpression || instruction);

  return (
    <ToolRow status={status} expandable={expandable}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Schedule Task</ToolRow.Title>
        <ToolRow.Subtitle>{title || null}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex flex-col gap-2">
          {cronExpression && (
            <ScheduleBlock
              cronExpression={cronExpression}
              nextRunAt={nextRunAt}
              timezone={timezone}
            />
          )}
          <div className="flex flex-col gap-3 px-3 pb-3 text-xs">
            {instruction && (
              <div className="flex flex-col gap-1">
                <p className="font-semibold text-text-secondary">Instruction</p>
                <p className="text-text-secondary whitespace-pre-wrap m-0">
                  {instruction}
                </p>
              </div>
            )}
            <div className="flex flex-wrap gap-x-4 gap-y-1 text-text-tertiary">
              <span>
                Mode <span className="text-text-secondary">{mode}</span>
              </span>
              {concurrency && (
                <span>
                  Concurrency{" "}
                  <span className="text-text-secondary">{concurrency}</span>
                </span>
              )}
              {targetAgent && (
                <span className="inline-flex items-center gap-1">
                  <UserCircleIcon className="h-3.5 w-3.5" />
                  <span className="text-text-secondary">{targetAgent}</span>
                </span>
              )}
            </div>
          </div>
        </div>
      </ToolRow.Body>
    </ToolRow>
  );
};
