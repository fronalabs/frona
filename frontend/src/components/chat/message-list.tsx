"use client";

import { useEffect, useRef, useMemo } from "react";
import { useChat } from "@/lib/chat-context";
import { useNavigation } from "@/lib/navigation-context";
import { MessageBubble } from "./message-bubble";
import { StreamingBubble } from "./streaming-bubble";
import { ToolMessage } from "./tool-message";

export function MessageList() {
  const { messages, streamingContent, activeToolCalls, activeChat } = useChat();
  const { agents } = useNavigation();

  const agent = agents.find((a) => a.id === activeChat?.agent_id);
  const agentName =
    agent?.name ?? (activeChat?.agent_id === "system" ? "Frona" : activeChat?.agent_id ?? "Assistant");
  const bottomRef = useRef<HTMLDivElement>(null);

  const visibleMessages = useMemo(
    () =>
      messages.filter(
        (m) =>
          m.tool ||
          (m.role !== "toolresult" &&
            !(m.role === "assistant" && !m.content && m.tool_calls)),
      ),
    [messages],
  );

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages, streamingContent, activeToolCalls]);

  return (
    <div className="flex-1 px-6 py-4 space-y-3">
      {visibleMessages.map((msg) => {
        if (msg.tool) {
          return <ToolMessage key={msg.id} message={msg} agentName={agentName} />;
        }
        return <MessageBubble key={msg.id} message={msg} agentName={agentName} />;
      })}
      {streamingContent !== null && (
        <StreamingBubble content={streamingContent} toolCalls={activeToolCalls} agentName={agentName} />
      )}
      <div ref={bottomRef} />
    </div>
  );
}
