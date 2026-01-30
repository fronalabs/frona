"use client";

import { useChat } from "@/lib/chat-context";
import { useNavigation } from "@/lib/navigation-context";

export function ChatHeader() {
  const { activeChat } = useChat();
  const { agents } = useNavigation();

  if (!activeChat) return null;

  const agent = agents.find((a) => a.id === activeChat.agent_id);
  const agentName =
    agent?.name ?? (activeChat.agent_id === "system" ? "Frona" : activeChat.agent_id);

  return (
    <div className="flex items-center border-b border-border px-6 py-3">
      <div>
        <h2 className="text-sm font-semibold text-text-primary">
          {activeChat.title ?? "New chat"}
        </h2>
        <p className="text-xs text-text-tertiary">{agentName}</p>
      </div>
    </div>
  );
}
