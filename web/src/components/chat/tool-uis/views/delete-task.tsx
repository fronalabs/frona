"use client";

import { CheckCircleIcon } from "@heroicons/react/24/outline";
import { shouldUseToolViewFallback, ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

interface ParsedResult {
  message?: string;
  title?: string;
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
  const message =
    typeof (obj as { message?: unknown }).message === "string"
      ? (obj as { message: string }).message
      : undefined;
  if (message === undefined) return null;
  const titleMatch = message?.match(/^Task '(.+)' cancelled\.$/);
  return { message, title: titleMatch ? titleMatch[1] : undefined };
}

export const DeleteTaskView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const taskId = typeof a.task_id === "string" ? a.task_id : "";

  const parsed = parseResult(result);
  if (shouldUseToolViewFallback(result, parsed !== null, status)) {
    return <ToolViewFallback />;
  }

  const taskTitle = parsed?.title ?? null;
  const message = parsed?.message ?? null;

  // Subtitle is the task title once the result arrives. Pre-result, show a
  // short hex slice of the task_id so the row isn't anonymous.
  const subtitle = taskTitle ?? (taskId ? taskId.slice(0, 8) : null);

  const expandable = taskId.length > 0 || message !== null;

  return (
    <ToolRow status={status} expandable={expandable}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Delete Task</ToolRow.Title>
        <ToolRow.Subtitle>{subtitle}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex flex-col gap-2 p-3 text-xs">
          {message && (
            <div className="flex items-start gap-2 text-text-secondary">
              <CheckCircleIcon className="h-4 w-4 shrink-0 text-success mt-px" />
              <span>{message}</span>
            </div>
          )}
          {taskId && (
            <p className="text-text-tertiary font-mono break-all m-0">
              ID: {taskId}
            </p>
          )}
        </div>
      </ToolRow.Body>
    </ToolRow>
  );
};
