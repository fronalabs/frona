import React, { createContext, useContext, useState } from "react";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ConfigurationSidebar } from "../configuration-sidebar";

const route = vi.hoisted(() => ({ mobile: true, pathname: "/agents", search: "id=agent-1&section=profile", push: vi.fn() }));
const Navigation = createContext({ mobileSubNavOpen: false, setMobileSubNavOpen: (_open: boolean) => {} });
vi.mock("@/lib/navigation-context", () => ({ useNavigation: () => useContext(Navigation) }));
vi.mock("@/lib/use-mobile", () => ({ useMobile: () => route.mobile }));
vi.mock("next/navigation", () => ({
  usePathname: () => route.pathname,
  useSearchParams: () => new URLSearchParams(route.search),
  useRouter: () => ({ push: route.push }),
}));

function Settings() {
  const [mobileSubNavOpen, setMobileSubNavOpen] = useState(false);
  const [activeSection, setActiveSection] = useState("profile");
  const [draft, setDraft] = useState("Unchanged draft");
  return <Navigation.Provider value={{ mobileSubNavOpen, setMobileSubNavOpen }}>
    <button id="mobile-navigation-toggle" aria-expanded={mobileSubNavOpen}
      aria-controls="configuration-navigation" onClick={() => setMobileSubNavOpen(true)}>Open navigation</button>
    <ConfigurationSidebar header={<h2>Agent configuration</h2>} backHref="/chat" backLabel="Back"
      sections={[{ id: "profile", label: "Profile" }, { id: "prompt", label: "Prompt" }]}
      activeSection={activeSection} onSectionChange={setActiveSection} />
    <input aria-label="Draft" value={draft} onChange={event => setDraft(event.target.value)} />
    <p role="alert">Existing save error</p>
    <output>{activeSection}</output>
  </Navigation.Provider>;
}

beforeEach(() => {
  vi.clearAllMocks();
  route.mobile = true;
  route.pathname = "/agents";
  route.search = "id=agent-1&section=profile";
});

describe("configuration navigation", () => {
  it.each(["Escape", "close", "outside"])("opens a named modal and returns focus after %s dismissal", async dismissal => {
    render(<Settings />);
    const opener = screen.getByRole("button", { name: "Open navigation" });
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "Keep my draft" } });
    fireEvent.click(opener);
    const dialog = screen.getByRole("dialog", { name: "Configuration navigation" });
    expect(opener).toHaveAttribute("aria-controls", dialog.id);
    expect(opener).toHaveAttribute("aria-expanded", "true");
    expect(dialog).toContainElement(document.activeElement as HTMLElement);
    expect(within(dialog).getByRole("button", { name: "Profile" })).toHaveAttribute("aria-current", "page");
    if (dismissal === "Escape") fireEvent.keyDown(dialog, { key: "Escape" });
    else if (dismissal === "close") fireEvent.click(screen.getByRole("button", { name: "Close configuration navigation" }));
    else {
      await act(() => new Promise(resolve => setTimeout(resolve, 0)));
      fireEvent(document.body, new MouseEvent("pointerdown", { bubbles: true, button: 0 }));
      fireEvent.click(document.body);
    }
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
    expect(screen.getByRole("textbox")).toHaveValue("Keep my draft");
    expect(screen.getByRole("alert")).toHaveTextContent("Existing save error");
    expect(route.push).not.toHaveBeenCalled();
  });

  it("selects a section, closes even when reselecting it, and retains the selection on reopening", async () => {
    render(<Settings />);
    const opener = screen.getByRole("button", { name: "Open navigation" });
    fireEvent.click(opener);
    fireEvent.click(screen.getByRole("button", { name: "Prompt" }));
    await waitFor(() => expect(opener).toHaveFocus());
    expect(screen.getByRole("status")).toHaveTextContent("prompt");
    fireEvent.click(opener);
    expect(screen.getByRole("button", { name: "Prompt" })).toHaveAttribute("aria-current", "page");
    fireEvent.click(screen.getByRole("button", { name: "Prompt" }));
    await waitFor(() => expect(opener).toHaveFocus());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("keeps focus within the open modal and preserves the Back destination", async () => {
    render(<Settings />);
    const outside = screen.getByRole("textbox");
    fireEvent.click(screen.getByRole("button", { name: "Open navigation" }));
    const dialog = screen.getByRole("dialog");
    act(() => outside.focus());
    expect(dialog).toContainElement(document.activeElement as HTMLElement);
    fireEvent.click(screen.getByRole("button", { name: "Back" }));
    expect(route.push).toHaveBeenCalledExactlyOnceWith("/chat");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("closes on external route changes and switches to the desktop sidebar without leaving an open modal", async () => {
    const view = render(<Settings />);
    const opener = screen.getByRole("button", { name: "Open navigation" });
    fireEvent.click(opener);
    route.search = "id=agent-2&section=profile";
    view.rerender(<Settings />);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    fireEvent.click(opener);
    route.mobile = false;
    view.rerender(<Settings />);
    expect(screen.getByRole("navigation", { name: "Configuration sections" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    route.mobile = true;
    view.rerender(<Settings />);
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    expect(opener).toHaveAttribute("aria-expanded", "false");
  });

  it("keeps desktop navigation inline and shows its current section", () => {
    route.mobile = false;
    render(<Settings />);
    const sidebar = screen.getByRole("complementary");
    fireEvent.click(within(sidebar).getByRole("button", { name: "Prompt" }));
    expect(within(sidebar).getByRole("button", { name: "Prompt" })).toHaveAttribute("aria-current", "page");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("textbox")).toHaveValue("Unchanged draft");
  });
});
