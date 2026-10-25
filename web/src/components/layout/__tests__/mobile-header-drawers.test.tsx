import React from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { TopBar } from "../top-bar";

const mocks = vi.hoisted(() => ({ push: vi.fn(), replace: vi.fn(), markRead: vi.fn(), markAllRead: vi.fn(), logout: vi.fn() }));
vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: mocks.push, replace: mocks.replace }),
  usePathname: () => window.location.pathname,
}));
vi.mock("@/lib/use-mobile", () => ({ useMobile: () => true }));
vi.mock("@/lib/auth", () => ({
  useAuth: () => ({ user: { email: "user@example.test", permissions: { is_admin: true, list_users: true } }, logout: mocks.logout }),
}));
vi.mock("@/lib/navigation-context", () => ({
  useNavigation: () => ({ agents: [{ id: "helper", name: "Helper", is_shared: true }] }),
}));
vi.mock("@/lib/session-context", () => ({ useSession: () => ({ setActiveChat: vi.fn() }) }));
vi.mock("@/lib/activity-context", () => ({ useActivity: () => ({ snapshot: { executions: [] } }) }));
vi.mock("@/lib/api-client", () => ({
  API_URL: "http://localhost:3001",
  getMemoryStatus: async () => true,
  api: { get: async () => [{ id: "app", handle: "app", name: "Example app", status: "serving", kind: "static" }] },
}));
vi.mock("@/lib/notification-context", () => ({
  useNotifications: () => ({
    unreadCount: 2, markRead: mocks.markRead, markAllRead: mocks.markAllRead,
    notifications: [
      { id: "chat", title: "Chat ready", read: false, level: "info", created_at: "2026-10-25T12:00:00Z", data: { type: "Agent", chat_id: "current" } },
      { id: "app", title: "App ready", read: false, level: "info", created_at: "2026-10-25T12:00:00Z", data: { type: "App", app_handle: "app", action: "start" } },
    ],
  }),
}));
vi.mock("../about-dialog", () => ({ AboutDialog: ({ open }: { open: boolean }) => open ? <div role="dialog">About Frona</div> : null }));

function tap(element: HTMLElement) {
  fireEvent.touchStart(element);
  fireEvent.touchEnd(element);
  fireEvent.click(element);
}

beforeEach(() => {
  vi.clearAllMocks();
  window.history.replaceState(null, "", "/chat?id=current");
});

describe("mobile header drawers", () => {
  it.each([
    ["Memory", "/memory"], ["Skills", "/settings#skills"], ["MCP", "/settings#mcp"],
    ["Vault", "/settings#vault"], ["Usage", "/usage"], ["Settings", "/settings"],
  ])("closes the account drawer on %s selection, even on the same route", async (label, href) => {
    window.history.replaceState(null, "", href);
    render(<TopBar />);
    tap(screen.getByTitle("user@example.test"));
    tap(await screen.findByRole("button", { name: label }));
    expect(screen.queryByText("Account", { exact: true })).not.toBeInTheDocument();
    if (href.includes("#")) expect(window.location.hash).toBe(href.slice(href.indexOf("#")));
    else expect(mocks.push).toHaveBeenCalledExactlyOnceWith(href);
  });

  it.each(["About", "Logout"])("closes the account drawer on %s", async label => {
    render(<TopBar />);
    tap(screen.getByTitle("user@example.test"));
    tap(await screen.findByRole("button", { name: label }));
    expect(screen.queryByText("Account", { exact: true })).not.toBeInTheDocument();
    if (label === "About") expect(screen.getByRole("dialog")).toHaveTextContent("About Frona");
    else expect(mocks.replace).toHaveBeenCalledExactlyOnceWith("/login");
  });

  it.each([
    ["Providers", "providers"], ["Models", "models"], ["Skills", "skills"], ["Vault", "vault"],
    ["Search", "search"], ["Voice", "voice"], ["Browser", "browser"], ["Sandbox", "sandbox"],
    ["Users", "users"], ["Server Settings", "providers"],
  ])("closes administration when reselecting %s", async (label, hash) => {
    window.history.replaceState(null, "", `/admin/settings#${hash}`);
    render(<TopBar />);
    tap(screen.getByTitle("Administration"));
    tap(await screen.findByRole("button", { name: label }));
    expect(screen.queryByRole("button", { name: label })).not.toBeInTheDocument();
    expect(window.location.hash).toBe(`#${hash}`);
  });

  it.each(["Chat ready", "App ready"])("closes notifications when opening %s", async label => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    render(<TopBar />);
    tap(screen.getByTitle("Notifications"));
    tap(await screen.findByRole("button", { name: new RegExp(label) }));
    expect(screen.queryByText(label)).not.toBeInTheDocument();
    if (label === "Chat ready") expect(mocks.push).toHaveBeenCalledExactlyOnceWith("/chat?id=current");
    else expect(open).toHaveBeenCalledExactlyOnceWith("http://localhost:3001/apps/app/", "_blank");
    open.mockRestore();
  });

  it("keeps notifications open when marking them read", async () => {
    render(<TopBar />);
    tap(screen.getByTitle("Notifications"));
    tap(await screen.findByRole("button", { name: "Mark all read" }));
    expect(mocks.markAllRead).toHaveBeenCalledOnce();
    expect(screen.getByText("Chat ready")).toBeInTheDocument();
  });

  it("closes Apps when opening an app", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    render(<TopBar />);
    tap(screen.getByTitle("Apps"));
    tap(await screen.findByText("Example app"));
    expect(screen.queryByText("Example app")).not.toBeInTheDocument();
    expect(open).toHaveBeenCalledExactlyOnceWith("http://localhost:3001/apps/app/", "_blank");
    open.mockRestore();
  });

  it.each([false, true])("closes Agents on selection (configure=%s)", async configure => {
    render(<TopBar />);
    const trigger = screen.getByTitle("Agents");
    tap(trigger);
    tap(configure ? await screen.findByRole("button", { name: "Configure Helper" }) : await screen.findByText("Helper"));
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(mocks.push).toHaveBeenCalledExactlyOnceWith(configure ? "/agents?id=helper" : "/chat?agent=helper");
  });
});
