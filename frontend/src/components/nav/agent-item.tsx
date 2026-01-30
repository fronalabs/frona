"use client";

import type { Agent } from "@/lib/types";

interface AgentItemProps {
  agent: Agent;
}

export function AgentItem({ agent }: AgentItemProps) {
  return (
    <div className="flex items-center justify-between rounded-lg px-3 py-2 text-sm hover:bg-surface-secondary transition">
      <span className="truncate text-text-primary">{agent.name}</span>
      <span
        className={`ml-2 h-2 w-2 shrink-0 rounded-full ${
          agent.enabled ? "bg-green-500" : "bg-text-tertiary"
        }`}
      />
    </div>
  );
}
