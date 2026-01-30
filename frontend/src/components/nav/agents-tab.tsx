"use client";

import { useNavigation } from "@/lib/navigation-context";
import { AgentItem } from "./agent-item";

export function AgentsTab() {
  const { agents } = useNavigation();

  return (
    <div className="space-y-1 p-2">
      {agents.map((agent) => (
        <AgentItem key={agent.id} agent={agent} />
      ))}
      {agents.length === 0 && (
        <p className="px-2 py-4 text-center text-xs text-text-tertiary">
          No agents configured
        </p>
      )}
    </div>
  );
}
