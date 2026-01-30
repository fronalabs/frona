"use client";

import { Suspense } from "react";
import { AuthGuard } from "@/components/auth/auth-guard";
import { NavigationProvider } from "@/lib/navigation-context";
import { ChatProvider } from "@/lib/chat-context";
import { NavigationPanel } from "@/components/layout/navigation-panel";
import { ConversationPanel } from "@/components/chat/conversation-panel";

export default function ChatLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <AuthGuard>
      <NavigationProvider>
        <Suspense>
          <ChatProvider>
            <div className="flex h-screen">
              <NavigationPanel />
              <ConversationPanel>{children}</ConversationPanel>
            </div>
          </ChatProvider>
        </Suspense>
      </NavigationProvider>
    </AuthGuard>
  );
}
