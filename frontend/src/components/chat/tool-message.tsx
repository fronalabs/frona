"use client";

import { useState } from "react";
import { useChat } from "@/lib/chat-context";
import type { MessageResponse } from "@/lib/types";

function QuestionMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  const { resolveToolMessage } = useChat();
  const [loading, setLoading] = useState(false);
  const [answered, setAnswered] = useState<string | null>(null);

  if (!message.tool || message.tool.type !== "Question") return null;

  const resolved = message.tool.data.status === "resolved";
  const selectedAnswer = answered ?? message.tool.data.response;

  const handleAnswer = async (answer: string) => {
    setLoading(true);
    setAnswered(answer);
    try {
      await resolveToolMessage(message.id, answer);
    } finally {
      setLoading(false);
    }
  };

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
          <p className="text-sm text-text-primary mb-2">
            {message.tool.data.question}
          </p>
          <div className="flex flex-col gap-2">
            {message.tool.data.options.map((option) => {
              const isSelected = selectedAnswer === option;
              return (
                <button
                  key={option}
                  onClick={() => handleAnswer(option)}
                  disabled={loading || resolved || answered !== null}
                  className={`rounded-lg border px-3 py-1.5 text-left text-sm font-medium transition ${
                    isSelected
                      ? "border-accent bg-accent/10 text-accent"
                      : resolved || answered !== null
                        ? "border-border text-text-tertiary opacity-50"
                        : "border-border text-text-secondary hover:border-accent hover:text-accent"
                  }`}
                >
                  {option}
                </button>
              );
            })}
          </div>
        </div>
      </div>
    </div>
  );
}

function HumanInTheLoopMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  const { resolveToolMessage } = useChat();
  const [loading, setLoading] = useState(false);

  if (!message.tool || message.tool.type !== "HumanInTheLoop") return null;

  const resolved = message.tool.data.status === "resolved";

  const handleResume = async () => {
    setLoading(true);
    try {
      await resolveToolMessage(message.id, "resumed");
    } finally {
      setLoading(false);
    }
  };

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
          <p className="text-sm text-text-primary mb-2">
            {message.tool.data.reason}
          </p>
          <div className="flex flex-wrap gap-2">
            {message.tool.data.debugger_url && (
              <a
                href={message.tool.data.debugger_url}
                target="_blank"
                rel="noopener noreferrer"
                className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium text-text-secondary hover:border-accent hover:text-accent transition"
              >
                Open Browser Debugger
              </a>
            )}
            <button
              onClick={handleResume}
              disabled={loading || resolved}
              className={`rounded-lg border px-3 py-1.5 text-sm font-medium transition ${
                resolved
                  ? "border-accent bg-accent/10 text-accent"
                  : "border-border text-text-secondary hover:border-accent hover:text-accent"
              }`}
            >
              {loading ? "Resuming..." : resolved ? "Resumed" : "Resume Agent"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

function WarningMessage({ message }: { message: MessageResponse }) {
  if (!message.tool || message.tool.type !== "Warning") return null;
  return (
    <div className="flex items-center gap-3 rounded-lg border border-yellow-200 bg-yellow-50 px-4 py-3 text-sm text-yellow-800 dark:border-yellow-800 dark:bg-yellow-950 dark:text-yellow-200">
      <span className="flex-1">{message.tool.data.message}</span>
    </div>
  );
}

function InfoMessage({ message }: { message: MessageResponse }) {
  if (!message.tool || message.tool.type !== "Info") return null;
  return (
    <div className="flex items-center gap-3 rounded-lg border border-blue-200 bg-blue-50 px-4 py-3 text-sm text-blue-800 dark:border-blue-800 dark:bg-blue-950 dark:text-blue-200">
      <span className="flex-1">{message.tool.data.message}</span>
    </div>
  );
}

export function ToolMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  if (!message.tool) return null;

  switch (message.tool.type) {
    case "Question":
      return <QuestionMessage message={message} agentName={agentName} />;
    case "HumanInTheLoop":
      return <HumanInTheLoopMessage message={message} agentName={agentName} />;
    case "Warning":
      return <WarningMessage message={message} />;
    case "Info":
      return <InfoMessage message={message} />;
    default:
      return null;
  }
}
