"use client";

import { useChat } from "@/lib/chat-context";
import { ChatHeader } from "./chat-header";
import { MessageList } from "./message-list";
import { MessageInput } from "./message-input";

export function ConversationPanel({ children }: { children?: React.ReactNode }) {
  const { activeChatId } = useChat();

  return (
    <div className="flex-1 overflow-y-auto bg-surface">
      {activeChatId ? (
        <div className="mx-auto flex min-h-full w-full max-w-3xl flex-col">
          <ChatHeader />
          <MessageList />
          <MessageInput />
        </div>
      ) : (
        children
      )}
    </div>
  );
}
