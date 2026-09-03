"use client";

import { Suspense } from "react";
import { AppGate } from "@/components/app-gate";
import { NavigationProvider } from "@/lib/navigation-context";
import { NotificationProvider } from "@/lib/notification-context";
import { SessionProvider } from "@/lib/session-context";
import { TopBar } from "@/components/layout/top-bar";
import { ActivityProvider } from "@/lib/activity-context";

export default function MainLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <NavigationProvider>
      <AppGate>
        <NotificationProvider>
          <Suspense>
            <SessionProvider>
              <ActivityProvider>
                <div className="flex flex-col h-screen">
                  <TopBar />
                  <div className="flex-1 overflow-hidden">
                    {children}
                  </div>
                </div>
              </ActivityProvider>
            </SessionProvider>
          </Suspense>
        </NotificationProvider>
      </AppGate>
    </NavigationProvider>
  );
}
