"use client";

import { useEffect, useRef, useMemo } from "react";
import { ChatBubbleLeftIcon } from "@heroicons/react/24/outline";
import { useSession } from "@/lib/session-context";
import { useNavigation } from "@/lib/navigation-context";
import { agentDisplayName } from "@/lib/types";
import { MessageBubble } from "./message-bubble";
import { StreamingBubble } from "./streaming-bubble";
import { ToolMessage } from "./tool-message";

export function MessageList() {
  const { messages, streamingContent, activeToolCalls, activeChat } = useSession();
  const { agents } = useNavigation();

  const agent = agents.find((a) => a.id === activeChat?.agent_id);
  const agentName = agentDisplayName(activeChat?.agent_id, agent?.name);
  const bottomRef = useRef<HTMLDivElement>(null);

  const visibleMessages = useMemo(
    () =>
      messages.filter(
        (m) =>
          m.tool ||
          (m.role !== "toolresult" &&
            !(m.role === "agent" && !m.content && m.tool_calls)),
      ),
    [messages],
  );

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages, streamingContent, activeToolCalls]);

  return (
    <div className="flex-1 px-6 py-4 space-y-3">
      {visibleMessages.map((msg) => {
        if (msg.tool && msg.tool.type === "TaskCompletion") {
          const { task_id } = msg.tool.data;
          return (
            <div key={msg.id} className="space-y-1">
              <MessageBubble message={msg} agentName={agentName} />
              {task_id && (
                <div className="pl-9.5 pt-1">
                  <a
                    href={`/chat?task=${task_id}`}
                    className="inline-flex items-center gap-1.5 rounded-lg border border-border px-3 py-1.5 text-xs font-medium text-text-secondary transition hover:border-accent hover:text-accent"
                  >
                    <ChatBubbleLeftIcon className="h-3.5 w-3.5" />
                    Ask Follow-up Questions
                  </a>
                </div>
              )}
            </div>
          );
        }
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
