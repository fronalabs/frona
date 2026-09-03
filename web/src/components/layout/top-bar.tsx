"use client";

import { useState, useRef, useEffect } from "react";
import { usePathname, useRouter } from "next/navigation";
import { Bars3Icon, XMarkIcon, Cog6ToothIcon, ArrowRightStartOnRectangleIcon, KeyIcon, CpuChipIcon, ChartBarIcon, CircleStackIcon, InformationCircleIcon, PuzzlePieceIcon } from "@heroicons/react/24/outline";
import { useAuth } from "@/lib/auth";
import { useSession } from "@/lib/session-context";
import { useNavigation } from "@/lib/navigation-context";
import { useMobile } from "@/lib/use-mobile";
import { useHashNavigate } from "@/lib/hash-navigate";
import { Logo } from "../logo";
import { NotificationDropdown } from "./notification-dropdown";
import { AgentDropdown } from "./agent-dropdown";
import { AppDropdown } from "./app-dropdown";
import { ServerDropdown } from "./server-dropdown";
import { AboutDialog } from "./about-dialog";
import { getMemoryStatus } from "@/lib/api-client";
import { useActivity } from "@/lib/activity-context";
import { ActivityIndicator } from "./activity-indicator";

const topTabs = [
  { id: "chat", label: "Assistant", href: "/chat" },
  { id: "files", label: "Files", href: "/files" },
] as const;

export function TopBar() {
  const router = useRouter();
  const pathname = usePathname();
  const { user, logout } = useAuth();
  const { setActiveChat } = useSession();
  const { snapshot: activity } = useActivity();
  const hasActiveExecutions = activity.executions.length > 0;
  const { mobileNavOpen, setMobileNavOpen, mobileSubNavOpen, setMobileSubNavOpen } = useNavigation();
  const mobile = useMobile();
  const hashNavigate = useHashNavigate();

  const [menuOpen, setMenuOpen] = useState(false);
  const [aboutOpen, setAboutOpen] = useState(false);
  const [memoryAvailable, setMemoryAvailable] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  const isChatRelated = pathname.startsWith("/chat") || pathname.startsWith("/home") || pathname.startsWith("/space");
  const activeTab = isChatRelated ? "chat" : (topTabs.find((t) => pathname.startsWith(t.href))?.id ?? null);

  useEffect(() => {
    if (!menuOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [menuOpen]);

  useEffect(() => {
    if (!user) return;
    let cancelled = false;
    getMemoryStatus()
      .then((available) => { if (!cancelled) setMemoryAvailable(available); })
      .catch(() => { if (!cancelled) setMemoryAvailable(false); });
    return () => { cancelled = true; };
  }, [user]);

  const handleLogout = () => {
    logout();
    router.replace("/login");
  };

  const initial = user?.email?.charAt(0)?.toUpperCase() ?? "?";

  if (mobile) {
    return (
      <>
      <AboutDialog open={aboutOpen} onClose={() => setAboutOpen(false)} />
      <div className="flex items-center h-14 px-3 bg-surface-nav border-b border-border shrink-0 gap-2">
        <button
          onClick={() => {
            if (pathname.startsWith("/settings")) {
              setMobileSubNavOpen(!mobileSubNavOpen);
            } else {
              setMobileNavOpen(!mobileNavOpen);
            }
          }}
          className="flex items-center justify-center h-10 w-10 rounded-lg text-text-secondary hover:text-text-primary hover:bg-surface-tertiary transition"
        >
          <Bars3Icon className="h-6 w-6" />
        </button>

        <button
          onClick={() => { setActiveChat(null); router.push("/home"); }}
          className="flex items-center gap-1 cursor-pointer"
        >
          <Logo size={52} />
          <span
            className="text-lg font-bold text-text-primary tracking-wide"
            style={{ fontFamily: "var(--font-brand)" }}
          >
            FRONA
          </span>
        </button>

        <div className="flex-1" />

        {hasActiveExecutions && <ActivityIndicator compact />}

        <div className="flex items-center gap-1">
          <AppDropdown />
          <NotificationDropdown />
          <ServerDropdown />
          <button
            onClick={() => setMenuOpen(true)}
            className="relative flex items-center justify-center h-10 w-10 text-surface text-sm font-semibold cursor-pointer rounded-full bg-accent hover:bg-accent-hover transition"
            title={user?.email ?? "User"}
          >
            {initial}
          </button>
        </div>

        {menuOpen && (
          <>
          <div className="fixed inset-0 z-[69] bg-black/40" onClick={() => setMenuOpen(false)} />
          <div className="fixed inset-y-0 right-0 z-[70] w-[85vw] max-w-sm bg-surface shadow-xl flex flex-col transition-transform duration-200 ease-out">
            <div className="flex items-center justify-between px-4 py-3 border-b border-border shrink-0">
              <span className="text-base font-semibold text-text-primary">Account</span>
              <button
                onClick={() => setMenuOpen(false)}
                className="flex items-center justify-center h-10 w-10 rounded-lg text-text-secondary hover:text-text-primary hover:bg-surface-tertiary transition"
              >
                <XMarkIcon className="h-5 w-5" />
              </button>
            </div>
            <div className="flex-1 overflow-y-auto">
              {user && (
                <div className="px-5 py-4 border-b border-border">
                  <span className="text-sm text-text-secondary">{user.email}</span>
                </div>
              )}
              <div className="py-2">
                {memoryAvailable && (
                  <button
                    onClick={() => { router.push("/memory"); setMenuOpen(false); }}
                    className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                  >
                    <CircleStackIcon className="h-5 w-5" />
                    Memory
                  </button>
                )}
                <button
                  onClick={() => { hashNavigate("/settings#skills"); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <PuzzlePieceIcon className="h-5 w-5" />
                  Skills
                </button>
                <button
                  onClick={() => { hashNavigate("/settings#mcp"); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <CpuChipIcon className="h-5 w-5" />
                  MCP
                </button>
                <button
                  onClick={() => { hashNavigate("/settings#vault"); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <KeyIcon className="h-5 w-5" />
                  Vault
                </button>
                <div className="border-t border-border my-1" />
                <button
                  onClick={() => { router.push("/usage"); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <ChartBarIcon className="h-5 w-5" />
                  Usage
                </button>
                <button
                  onClick={() => { hashNavigate("/settings"); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <Cog6ToothIcon className="h-5 w-5" />
                  Settings
                </button>
                <button
                  onClick={() => { handleLogout(); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <ArrowRightStartOnRectangleIcon className="h-5 w-5" />
                  Logout
                </button>
                <button
                  onClick={() => { setAboutOpen(true); setMenuOpen(false); }}
                  className="w-full flex items-center gap-3 px-5 py-3 text-base text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <InformationCircleIcon className="h-5 w-5" />
                  About
                </button>
              </div>
            </div>
          </div>
          </>
        )}
      </div>
      </>
    );
  }

  return (
    <>
    <AboutDialog open={aboutOpen} onClose={() => setAboutOpen(false)} />
    <div className="flex items-stretch h-20 pr-5 bg-surface-nav border-b border-border shrink-0">
      <div className="flex items-center justify-center shrink-0" style={{ width: 288 }}>
        <button
          onClick={() => { setActiveChat(null); router.push("/home"); }}
          className="flex items-center gap-1 cursor-pointer"
        >
          <Logo size={68} />
          <span
            className="text-2xl font-bold text-text-primary tracking-wide"
            style={{ fontFamily: "var(--font-brand)" }}
          >
            FRONA
          </span>
        </button>
      </div>

      <div className="flex items-end self-end gap-1">
        {topTabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => router.push(tab.href)}
            className={`w-28 text-center py-2 text-base font-medium transition rounded-t-lg border border-b-0 relative ${
              activeTab === tab.id
                ? "bg-surface border-border text-text-primary mb-[-1px] pb-[calc(0.5rem+1px)]"
                : "bg-transparent border-transparent text-text-secondary hover:text-text-primary hover:bg-surface-tertiary/50"
            }`}
          >
            {tab.label}
          </button>
        ))}
      </div>

      <div className="flex-1" />

      {hasActiveExecutions && (
        <div className="mr-2 flex items-center">
          <ActivityIndicator />
        </div>
      )}

      <div className="flex items-center gap-2">
      <AgentDropdown />
      <AppDropdown />
      <NotificationDropdown />
      <ServerDropdown />
      <div ref={menuRef} className="relative flex items-center">
        <button
          onClick={() => setMenuOpen((v) => !v)}
          className={`relative flex items-center justify-center h-10 w-10 text-surface text-sm font-semibold cursor-pointer transition ${
            menuOpen ? "rounded-t-xl rounded-b-none bg-surface-secondary text-text-primary z-[61] border border-border border-b-0" : "rounded-full bg-accent hover:bg-accent-hover"
          }`}
          title={user?.email ?? "User"}
        >
          {initial}
        </button>

        {menuOpen && (
          <div className="absolute right-0 top-full z-[60] w-52 rounded-xl rounded-tr-none border border-border bg-surface-secondary shadow-lg">
            <div className="absolute -top-px right-0 w-[calc(theme(spacing.10)-3px)] h-[2px] bg-surface-secondary z-[60]" />
            <div className="pb-1">
              {user && (
                <div className="flex items-center justify-between px-4 py-2 border-b border-border shrink-0">
                  <span className="text-sm font-medium text-text-secondary truncate">{user.email}</span>
                </div>
              )}
              {memoryAvailable && (
                <button
                  onClick={() => { router.push("/memory"); setMenuOpen(false); }}
                  className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
                >
                  <CircleStackIcon className="h-4 w-4" />
                  Memory
                </button>
              )}
              <button
                onClick={() => { hashNavigate("/settings#skills"); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <PuzzlePieceIcon className="h-4 w-4" />
                Skills
              </button>
              <button
                onClick={() => { hashNavigate("/settings#mcp"); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <CpuChipIcon className="h-4 w-4" />
                MCP
              </button>
              <button
                onClick={() => { hashNavigate("/settings#vault"); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <KeyIcon className="h-4 w-4" />
                Vault
              </button>
              <div className="border-t border-border" />
              <button
                onClick={() => { router.push("/usage"); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <ChartBarIcon className="h-4 w-4" />
                Usage
              </button>
              <button
                onClick={() => { hashNavigate("/settings"); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <Cog6ToothIcon className="h-4 w-4" />
                Settings
              </button>
              <button
                onClick={() => { handleLogout(); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <ArrowRightStartOnRectangleIcon className="h-4 w-4" />
                Logout
              </button>
              <button
                onClick={() => { setAboutOpen(true); setMenuOpen(false); }}
                className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
              >
                <InformationCircleIcon className="h-4 w-4" />
                About
              </button>
            </div>
          </div>
        )}
      </div>
      </div>
    </div>
    </>
  );
}
