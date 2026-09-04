import type { Execution } from "./types";

const kindLabels: Record<Execution["kind"], string> = {
  inference: "Inference",
  task: "Task",
  memory: "Memory",
  app: "App",
  scheduled: "Scheduled",
  system: "System",
};

const statusLabels: Record<Execution["status"], string> = {
  queued: "Queued",
  running: "Running",
  waiting: "Waiting",
  cancelling: "Cancelling",
};

export function executionKindLabel(execution: Execution): string {
  return kindLabels[execution.kind];
}

export function executionMetadataLabel(execution: Execution): string {
  return [
    execution.agentName,
    executionKindLabel(execution),
    statusLabels[execution.status],
  ]
    .filter(Boolean)
    .join(" - ");
}
