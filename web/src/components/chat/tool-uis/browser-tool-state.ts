import type { ToolCallMessagePart, ToolCallMessagePartStatus } from "@assistant-ui/react";

export function isBrowserTool(name: string): boolean {
  return name.startsWith("browser_");
}

export function browserNavigationWarnings(result: unknown): string[] {
  try {
    const value = typeof result === "string" ? JSON.parse(result) : result;
    return value && Array.isArray(value.warnings)
      ? value.warnings.filter((warning: unknown): warning is string => typeof warning === "string")
      : [];
  } catch {
    return [];
  }
}

export function browserCallStatus(
  call: ToolCallMessagePart,
  messageStatus: { type: string } | undefined,
): ToolCallMessagePartStatus {
  if (call.args?.isError === true) {
    return { type: "incomplete", reason: "error", error: call.result };
  }
  if (call.result !== undefined) return { type: "complete" };
  if (messageStatus?.type === "running") return { type: "running" };
  return { type: "incomplete", reason: "cancelled" };
}
