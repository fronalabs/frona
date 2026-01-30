"use client";

import { useRef, useState, useEffect } from "react";
import { PaperAirplaneIcon, StopIcon } from "@heroicons/react/24/solid";
import { useChat } from "@/lib/chat-context";

export function MessageInput() {
  const { sendMessage, stopGeneration, sending, activeChatId } = useChat();
  const [text, setText] = useState("");
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (activeChatId && !sending) {
      requestAnimationFrame(() => textareaRef.current?.focus());
    }
  }, [activeChatId, sending]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const content = text.trim();
    if (!content || !activeChatId) return;
    setText("");
    await sendMessage(content);
    // Re-focus after React re-renders with sending=false
    requestAnimationFrame(() => textareaRef.current?.focus());
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSubmit(e);
    }
  };

  return (
    <form onSubmit={handleSubmit} className="sticky bottom-0 bg-surface p-4">
      <div className="flex items-center gap-2 rounded-xl border border-border bg-surface-secondary px-3 py-2 focus-within:border-accent transition-colors">
        <textarea
          ref={textareaRef}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Send a message..."
          rows={1}
          className="flex-1 resize-none bg-transparent text-sm leading-5 text-text-primary placeholder:text-text-tertiary focus:outline-none m-0 p-0"
          disabled={!activeChatId || sending}
        />
        {sending ? (
          <button
            type="button"
            onClick={stopGeneration}
            className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary transition"
          >
            <StopIcon className="h-5 w-5" />
          </button>
        ) : (
          <button
            type="submit"
            disabled={!text.trim() || !activeChatId}
            className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary disabled:opacity-30 transition"
          >
            <PaperAirplaneIcon className="h-5 w-5" />
          </button>
        )}
      </div>
    </form>
  );
}
