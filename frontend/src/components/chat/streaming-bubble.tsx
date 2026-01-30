"use client";

import { MarkdownContent } from "./markdown-content";
import { ToolCallIndicator } from "./tool-call-indicator";
import type { ToolCallStatus } from "@/lib/types";

interface StreamingBubbleProps {
  content: string;
  toolCalls?: ToolCallStatus[];
  agentName: string;
}

export function StreamingBubble({ content, toolCalls, agentName }: StreamingBubbleProps) {
  const hasToolCalls = toolCalls && toolCalls.length > 0;

  return (
    <div className="flex justify-start">
      <div className="flex items-start gap-2.5 max-w-[85%]">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-surface-tertiary text-text-secondary">
          {agentName.charAt(0).toUpperCase()}
        </div>
        <div className="min-w-0 pt-0.5">
          <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
            {agentName}
          </p>
          {hasToolCalls && (
            <div className="flex flex-col gap-1 mb-1.5">
              {toolCalls.map((tc, i) => (
                <ToolCallIndicator key={`${tc.name}-${i}`} toolCall={tc} />
              ))}
            </div>
          )}
          <div className="text-sm text-text-primary">
            {content ? (
              <MarkdownContent content={content} />
            ) : !hasToolCalls ? (
              <p className="animate-pulse text-text-tertiary">...</p>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}
