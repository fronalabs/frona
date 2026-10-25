import React, { createContext, useContext, useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { NavigationPanel } from "../navigation-panel";

const route = vi.hoisted(() => ({ pathname: "/chat", search: "id=current", activeTab: "chat", push: vi.fn() }));
const Navigation = createContext({ mobileNavOpen: false, setMobileNavOpen: (_open: boolean) => {} });
vi.mock("@/lib/navigation-context", () => ({
  useNavigation: () => ({
    ...useContext(Navigation),
    activeTab: route.activeTab,
    tasks: [{ id: "task", title: "Selected task", status: "pending", kind: { type: "Direct" } }],
    spaces: [{ id: "space", name: "Project space", chats: [] }],
    standaloneChats: [{ id: "current", title: "Current chat" }, { id: "other", title: "Other chat" }],
    archivedChats: [{ id: "archived", title: "Archived chat" }],
    showArchived: true,
    setShowArchived: vi.fn(),
  }),
}));
vi.mock("@/lib/use-mobile", () => ({ useMobile: () => true }));
vi.mock("@/lib/session-context", () => ({
  useSession: () => ({ activeChatId: "current", activeChat: null, setActiveChat: vi.fn() }),
}));
vi.mock("next/navigation", () => ({
  usePathname: () => route.pathname,
  useSearchParams: () => new URLSearchParams(route.search),
  useRouter: () => ({ push: route.push }),
}));

function MobileNavigation() {
  const [mobileNavOpen, setMobileNavOpen] = useState(true);
  return <Navigation.Provider value={{ mobileNavOpen, setMobileNavOpen }}>
    <button aria-expanded={mobileNavOpen} onClick={() => setMobileNavOpen(true)}>Open navigation</button>
    <NavigationPanel />
  </Navigation.Provider>;
}

function tap(element: HTMLElement) {
  fireEvent.touchStart(element, { touches: [{ identifier: 1, clientX: 50, clientY: 100 }] });
  fireEvent.touchEnd(element, { touches: [] });
  fireEvent.click(element);
}

beforeEach(() => {
  vi.clearAllMocks();
  route.pathname = "/chat";
  route.search = "id=current";
  route.activeTab = "chat";
});

describe("mobile chat navigation", () => {
  describe.each([
    ["touch", tap],
    ["mouse", (element: HTMLElement) => fireEvent.click(element)],
  ] as const)("%s activation", (_input, activate) => {
    it.each(["", "task=task"])("closes after selecting a task from query '%s'", search => {
      route.activeTab = "tasks";
      route.search = search;
      render(<MobileNavigation />);
      activate(screen.getByRole("button", { name: "Selected task" }));
      expect(route.push).toHaveBeenCalledExactlyOnceWith("/chat?task=task");
      expect(screen.getByRole("button", { name: "Open navigation" })).toHaveAttribute("aria-expanded", "false");
    });

    it("closes when selecting Files on the current route", () => {
      route.pathname = "/files";
      route.search = "";
      render(<MobileNavigation />);
      activate(screen.getByRole("button", { name: "Files" }));
      expect(route.push).toHaveBeenCalledExactlyOnceWith("/files");
      expect(screen.getByRole("button", { name: "Open navigation" })).toHaveAttribute("aria-expanded", "false");
    });

    it.each([
      ["Current chat", "/chat?id=current"],
      ["Other chat", "/chat?id=other"],
      ["Archived chat", "/chat?id=archived"],
      ["Project space", "/space?id=space"],
      ["New chat", "/chat"],
    ])("closes after selecting %s without waiting for a route change", (name, destination) => {
      if (name === "New chat") route.search = "";
      render(<MobileNavigation />);
      activate(name === "New chat" ? screen.getByTitle(name) : screen.getByRole("button", { name: new RegExp(`^${name}`) }));
      expect(route.push).toHaveBeenCalledExactlyOnceWith(destination);
      const opener = screen.getByRole("button", { name: "Open navigation" });
      expect(opener).toHaveAttribute("aria-expanded", "false");
      fireEvent.click(opener);
      expect(opener).toHaveAttribute("aria-expanded", "true");
    });
  });

  it("keeps the drawer open when using row actions or expanding archived chats", () => {
    render(<MobileNavigation />);
    tap(screen.getAllByRole("button", { name: "Chat actions" })[0]);
    expect(screen.getByRole("button", { name: "Archive" })).toBeInTheDocument();
    tap(screen.getByRole("button", { name: "Archived" }));
    expect(route.push).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Open navigation" })).toHaveAttribute("aria-expanded", "true");
  });

  it("keeps the drawer open during a touch scroll without activating a chat", () => {
    render(<MobileNavigation />);
    const chat = screen.getByRole("button", { name: "Other chat" });
    fireEvent.touchStart(chat, { touches: [{ identifier: 1, clientX: 50, clientY: 100 }] });
    fireEvent.touchMove(chat, { touches: [{ identifier: 1, clientX: 50, clientY: 50 }] });
    fireEvent.touchEnd(chat, { touches: [] });
    expect(route.push).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Open navigation" })).toHaveAttribute("aria-expanded", "true");
  });

  it("still closes when the route changes outside the drawer", () => {
    const view = render(<MobileNavigation />);
    route.search = "id=other";
    view.rerender(<MobileNavigation />);
    expect(screen.getByRole("button", { name: "Open navigation" })).toHaveAttribute("aria-expanded", "false");
  });
});
