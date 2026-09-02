"use client";

import type { ReactNode } from "react";
import { CodeBlock } from "@/components/ui/code-block";
import { cn } from "@/lib/utils";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

const TOOL_DISPLAY_NAMES: Record<string, string> = {
  web_fetch: "Web Fetch",
  web_search: "Web Search",
  cli: "Terminal",
  shell: "Shell",
  python: "Python",
  browser_navigate: "Browser",
  manage_app: "App",
  request_credentials: "Request Credentials",
  produce_file: "Produce File",
  store_agent_memory: "Remember",
  store_user_memory: "Remember",
  memory_search: "Memory Search",
  memory_graph_sparql: "Memory Graph Query",
  create_task: "Create Task",
  list_tasks: "List Tasks",
  delete_task: "Delete Task",
  task_control: "Task Control",
  complete_task: "Complete Task",
  defer_task: "Defer Task",
  fail_task: "Fail Task",
  update_identity: "Update Identity",
  update_entity: "Update Entity",
  set_heartbeat: "Set Heartbeat",
  notify_human: "Notify",
  request_user_takeover: "Request Takeover",
  ask_user_question: "Ask Question",
  make_voice_call: "Voice Call",
  send_dtmf: "Send DTMF",
  hangup_call: "Hang Up",
};

export function displayToolName(name: string): string {
  if (TOOL_DISPLAY_NAMES[name]) return TOOL_DISPLAY_NAMES[name];
  const bare = name.includes("__") ? name.split("__").pop()! : name;
  return bare.replace(/_/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());
}

/**
 * Build the args block. For plain JSON objects, strip the outer `{}` since
 * tool args are always an object — those braces are visual noise. Falls back
 * to a plain `<pre>` when argsText isn't valid JSON. Returns null when the
 * args are empty (`{}` or empty string).
 */
function buildArgsBlock(argsText: string): ReactNode {
  if (!argsText) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(argsText);
  } catch {
    return (
      <pre className="whitespace-pre-wrap text-text-secondary text-xs overflow-x-auto px-3 pt-3">
        {argsText}
      </pre>
    );
  }
  let formatted: string;
  if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) {
    const json = JSON.stringify(parsed, null, 2);
    if (json === "{}") return null;
    // Drop the leading "{\n" and trailing "\n}" then dedent two spaces per line.
    formatted = json.slice(2, -2).replace(/^ {2}/gm, "");
  } else {
    formatted = JSON.stringify(parsed, null, 2);
  }
  return <CodeBlock code={formatted} language="json" />;
}

export const DefaultView: ToolView = ({
  toolName,
  args,
  argsText,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const description =
    args && typeof (args as { description?: unknown }).description === "string"
      ? (args as { description: string }).description
      : null;
  const subtitle = description && description !== toolName ? description : null;

  const argsBlock = buildArgsBlock(argsText ?? "");
  const hasResult = result !== undefined;
  const expandable = argsBlock !== null || hasResult;

  const errorValue =
    status?.type === "incomplete"
      ? (status as { error?: unknown }).error
      : undefined;
  const errorText =
    errorValue == null
      ? null
      : typeof errorValue === "string"
        ? errorValue
        : JSON.stringify(errorValue);

  return (
    <ToolRow status={status} expandable={expandable}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>{displayToolName(toolName)}</ToolRow.Title>
        <ToolRow.Subtitle>{subtitle}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex flex-col gap-2">
          {argsBlock}
          {hasResult && (
            <div className={cn("text-xs", argsBlock ? "px-3 pb-3" : "p-3")}>
              <p className="font-semibold text-text-secondary">Result:</p>
              <pre className="whitespace-pre-wrap text-text-secondary overflow-x-auto mt-1">
                {typeof result === "string" ? result : JSON.stringify(result, null, 2)}
              </pre>
            </div>
          )}
        </div>
      </ToolRow.Body>

      <ToolRow.Error>
        <div className="flex flex-col gap-2">
          {argsBlock}
          {errorText != null && (
            <div className={cn("text-xs", argsBlock ? "px-3 pb-3" : "p-3")}>
              <p className="font-semibold text-danger">Failed</p>
              <pre className="whitespace-pre-wrap text-text-tertiary mt-1">
                {errorText}
              </pre>
            </div>
          )}
        </div>
      </ToolRow.Error>
    </ToolRow>
  );
};
