"use client";

import type { MessageResponse } from "@/lib/types";
import { MarkdownContent } from "./markdown-content";

interface MessageBubbleProps {
  message: MessageResponse;
  agentName: string;
}

export function MessageBubble({ message, agentName }: MessageBubbleProps) {
  const isUser = message.role === "user";

  return (
    <div className="flex justify-start">
      <div className="flex items-start gap-2.5 max-w-[85%]">
        <div
          className={`flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium ${
            isUser
              ? "bg-accent text-surface"
              : "bg-surface-tertiary text-text-secondary"
          }`}
        >
          {isUser ? "U" : agentName.charAt(0).toUpperCase()}
        </div>
        <div className="min-w-0 pt-0.5">
          <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
            {isUser ? "You" : agentName}
          </p>
          <div className="text-sm text-text-primary">
            {isUser ? (
              <p className="whitespace-pre-wrap">{message.content}</p>
            ) : (
              <MarkdownContent content={message.content} />
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
