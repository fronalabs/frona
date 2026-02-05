"use client";

import { useState } from "react";
import { useSession } from "@/lib/session-context";
import type { MessageResponse } from "@/lib/types";

function QuestionMessage({
  message,
  agentName,
}: {
  message: MessageResponse;
  agentName: string;
}) {
  const { resolveToolMessage } = useSession();
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
  const { resolveToolMessage } = useSession();
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

function TaskCompletionMessage({ message }: { message: MessageResponse }) {
  if (!message.tool || message.tool.type !== "TaskCompletion") return null;

  const { status } = message.tool.data;
  const isError = status === "failed";

  return (
    <div
      className={`flex items-start gap-3 rounded-lg border px-4 py-3 text-sm ${
        isError
          ? "border-red-200 bg-red-50 text-red-800 dark:border-red-800 dark:bg-red-950 dark:text-red-200"
          : "border-green-200 bg-green-50 text-green-800 dark:border-green-800 dark:bg-green-950 dark:text-green-200"
      }`}
    >
      <span className="flex-1">{message.content}</span>
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
    case "TaskCompletion":
      return <TaskCompletionMessage message={message} />;
    default:
      return null;
  }
}
