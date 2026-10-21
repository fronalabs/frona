"use client";

import { useState, useCallback, useRef, useEffect } from "react";
import { usePathname, useSearchParams, useRouter } from "next/navigation";
import { ChevronLeftIcon, Bars3Icon, XMarkIcon, FolderIcon } from "@heroicons/react/24/outline";
import { useNavigation } from "@/lib/navigation-context";
import { useMobile } from "@/lib/use-mobile";
import { TabBar } from "./tab-bar";
import { ChatsTab } from "../nav/chats-tab";
import { TasksTab } from "../nav/tasks-tab";

const MIN_WIDTH = 200;
const MAX_WIDTH = 480;
const DEFAULT_WIDTH = 289;
const COOKIE_NAME = "nav_collapsed";

function getCookie(name: string): string | null {
  const match = document.cookie.match(new RegExp(`(?:^|; )${name}=([^;]*)`));
  return match ? decodeURIComponent(match[1]) : null;
}

function setCookie(name: string, value: string, days = 365) {
  const expires = new Date(Date.now() + days * 864e5).toUTCString();
  document.cookie = `${name}=${encodeURIComponent(value)}; expires=${expires}; path=/`;
}

function NavigationContent({ activeTab }: { activeTab: string }) {
  return (
    <div className="flex-1 min-h-0 min-w-0 overflow-y-auto">
      {activeTab === "chat" && <ChatsTab />}
      {activeTab === "tasks" && <TasksTab />}
    </div>
  );
}

export function NavigationPanel() {
  const { activeTab, mobileNavOpen, setMobileNavOpen } = useNavigation();
  const mobile = useMobile();
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const router = useRouter();

  const [collapsed, setCollapsed] = useState(() => getCookie(COOKIE_NAME) === "1");
  const [width, setWidth] = useState(DEFAULT_WIDTH);
  const dragging = useRef(false);
  const panelRef = useRef<HTMLDivElement>(null);

  // Close mobile nav on any route change (pathname or query params)
  const prevUrl = useRef(pathname + searchParams.toString());
  useEffect(() => {
    const url = pathname + searchParams.toString();
    if (url !== prevUrl.current) {
      prevUrl.current = url;
      if (mobileNavOpen) setMobileNavOpen(false);
    }
  }, [pathname, searchParams, mobileNavOpen, setMobileNavOpen]);

  const toggleCollapsed = useCallback(() => {
    setCollapsed((prev) => {
      const next = !prev;
      setCookie(COOKIE_NAME, next ? "1" : "0");
      return next;
    });
  }, []);

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  }, []);

  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (!dragging.current) return;
      const newWidth = Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, e.clientX));
      setWidth(newWidth);
    };

    const onMouseUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };

    window.addEventListener("mousemove", onMouseMove);
    window.addEventListener("mouseup", onMouseUp);
    return () => {
      window.removeEventListener("mousemove", onMouseMove);
      window.removeEventListener("mouseup", onMouseUp);
    };
  }, []);

  if (mobile) {
    return (
      <>
        {/* Backdrop */}
        {mobileNavOpen && (
          <div
            className="fixed inset-0 z-40 bg-black/40 transition-opacity"
            onClick={() => setMobileNavOpen(false)}
          />
        )}
        {/* Drawer */}
        <div
          className={`fixed inset-y-0 left-0 z-50 flex flex-col w-[85vw] max-w-sm bg-surface-nav border-r border-border shadow-xl transition-transform duration-200 ease-out ${
            mobileNavOpen ? "translate-x-0" : "-translate-x-full"
          }`}
        >
          <div className="relative border-b border-border shrink-0">
            <div className="w-[calc(100%-2.5rem)]">
              <TabBar />
            </div>
            <button
              onClick={() => setMobileNavOpen(false)}
              className="absolute right-2 top-1/2 -translate-y-1/2 flex items-center justify-center h-10 w-10 rounded-lg text-text-secondary hover:text-text-primary hover:bg-surface-tertiary transition"
            >
              <XMarkIcon className="h-5 w-5" />
            </button>
          </div>
          <NavigationContent activeTab={activeTab} />
          <div className="border-t border-border p-2">
            <button
              onClick={() => { router.push("/files"); setMobileNavOpen(false); }}
              className={`flex w-full items-center gap-2 rounded-lg px-3 py-2.5 text-sm font-medium transition ${
                pathname.startsWith("/files")
                  ? "bg-surface-tertiary text-text-primary"
                  : "text-text-secondary hover:bg-surface-secondary hover:text-text-primary"
              }`}
            >
              <FolderIcon className="h-5 w-5" />
              Files
            </button>
          </div>
        </div>
      </>
    );
  }

  if (collapsed) {
    return (
      <button
        type="button"
        aria-label="Expand navigation panel"
        onClick={toggleCollapsed}
        className="group/nav relative flex h-full flex-col items-center border-r border-border bg-surface-nav w-6 shrink-0 cursor-pointer transition-colors hover:bg-surface-tertiary/30"
      >
        <Bars3Icon className="h-4 w-4 mt-3 text-text-tertiary" />
      </button>
    );
  }

  return (
    <div
      ref={panelRef}
      className="group/nav relative flex h-full flex-col border-r border-border bg-surface-nav shrink-0"
      style={{ width }}
    >
      <div className="flex items-center border-b border-border">
        <div className="flex-1"><TabBar /></div>
      </div>

      {/* Collapse control also remains available to touch and keyboard users. */}
      <button
        onClick={toggleCollapsed}
        className="absolute top-1/2 -translate-y-1/2 -right-3 z-10 flex items-center justify-center h-6 w-6 rounded-full border border-border bg-surface shadow-sm text-text-tertiary hover:text-text-primary hover:bg-surface-secondary transition row-action group-hover/nav:opacity-100 group-focus-within/nav:opacity-100 focus:opacity-100"
        title="Collapse panel"
      >
        <ChevronLeftIcon className="h-3 w-3" />
      </button>

      <NavigationContent activeTab={activeTab} />

      {/* Resize handle */}
      <div
        onMouseDown={onMouseDown}
        className="absolute top-0 right-0 bottom-0 w-1 cursor-col-resize hover:bg-accent/20 active:bg-accent/30 transition-colors"
      />
    </div>
  );
}
