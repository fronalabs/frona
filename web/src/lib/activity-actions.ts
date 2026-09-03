import { cancelGeneration, cancelTask } from "@/lib/api-client";
import type { Execution } from "@/lib/types";

export function executionHref(execution: Execution): string | null {
  if (execution.kind === "memory") {
    return "/memory?consolidation=expanded";
  }
  if (!execution.source?.id) return null;
  switch (execution.source.type) {
    case "chat":
      return `/chat?id=${execution.source.id}`;
    case "task":
    case "schedule":
      return `/chat?task=${execution.source.id}`;
    case "system":
      return null;
  }
}

export async function cancelExecution(execution: Execution): Promise<void> {
  if (!execution.source?.id) return;
  switch (execution.source.type) {
    case "chat":
      await cancelGeneration(execution.source.id);
      break;
    case "task":
    case "schedule":
      await cancelTask(execution.source.id);
      break;
    case "system":
      break;
  }
}
