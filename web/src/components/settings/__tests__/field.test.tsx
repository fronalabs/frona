import React from "react";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { HelpTip, SectionPanel, TextInput, Toggle } from "../field";

const description = "Keep the conversation history on this server.";

// jsdom does not populate pointerType on its PointerEvent fallback.
function pointer(target: Element, type: string, pointerType: string) {
  const event = new Event(type, { bubbles: true });
  Object.defineProperty(event, "pointerType", { value: pointerType });
  fireEvent(target, event);
}

describe("settings help", () => {
  beforeEach(() => {
    vi.stubGlobal("ResizeObserver", class {
      observe() {}
      unobserve() {}
      disconnect() {}
    });
  });
  afterEach(() => vi.unstubAllGlobals());

  it("opens a named, described portal on click and toggles closed", async () => {
    const { container } = render(<HelpTip content={description} />);
    const trigger = screen.getByRole("button", { name: "Help" });
    expect(trigger).toHaveAttribute("aria-haspopup", "dialog");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Help" });
    expect(dialog).toHaveAccessibleDescription(description);
    expect(trigger).toHaveAttribute("aria-controls", dialog.id);
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(container).not.toContainElement(dialog);
    expect(dialog).toHaveFocus();
    fireEvent.click(trigger);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(trigger).toHaveFocus());
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("returns focus on Escape without reopening the hover tooltip", async () => {
    render(<HelpTip label="Help for history" content={description} />);
    const trigger = screen.getByRole("button", { name: "Help for history" });
    act(() => trigger.focus());
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
    // Native button keyboard activation produces click; real Enter/Space are also browser-tested.
    fireEvent.click(trigger, { detail: 0 });
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    await waitFor(() => expect(trigger).toHaveFocus());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("dismisses on outside pointer interaction and keeps focus on the outside control", async () => {
    render(<><HelpTip content={description} /><button>Other setting</button></>);
    fireEvent.click(screen.getByRole("button", { name: "Help" }));
    // Radix installs its outside-pointer listener after the opening event.
    await act(() => new Promise(resolve => setTimeout(resolve, 0)));
    const outside = screen.getByRole("button", { name: "Other setting" });
    pointer(outside, "pointerdown", "mouse");
    act(() => outside.focus());
    fireEvent.click(outside);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(outside).toHaveFocus();
  });

  it("dismisses when keyboard focus leaves the popover", async () => {
    render(<><HelpTip content={description} /><input aria-label="Other setting" /></>);
    fireEvent.click(screen.getByRole("button", { name: "Help" }));
    const outside = screen.getByRole("textbox");
    act(() => outside.focus());
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(outside).toHaveFocus();
  });

  it("retains delayed mouse hover and replaces the tooltip with the activated popover", async () => {
    render(<HelpTip content={description} />);
    const trigger = screen.getByRole("button", { name: "Help" });
    pointer(trigger, "pointermove", "mouse");
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
    expect(await screen.findByRole("tooltip")).toHaveTextContent(description);
    expect(trigger).not.toHaveFocus();
    fireEvent.click(trigger);
    expect(screen.getByRole("dialog")).toHaveAccessibleDescription(description);
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("does not queue hover help while the popover is open", async () => {
    render(<HelpTip content={description} />);
    const trigger = screen.getByRole("button", { name: "Help" });
    fireEvent.click(trigger);
    pointer(trigger, "pointermove", "mouse");
    await act(() => new Promise(resolve => setTimeout(resolve, 250)));
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    await waitFor(() => expect(trigger).toHaveFocus());
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("uses touch activation without requiring hover", async () => {
    render(<HelpTip content={description} />);
    const trigger = screen.getByRole("button", { name: "Help" });
    pointer(trigger, "pointermove", "touch");
    await act(() => new Promise(resolve => setTimeout(resolve, 250)));
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
    fireEvent.click(trigger);
    expect(screen.getByRole("dialog")).toHaveAccessibleDescription(description);
  });

  it("names help for fields and panels without changing the paired input or switch", async () => {
    const onChange = vi.fn();
    render(<><TextInput label="History path" description={description} value="/history" onChange={onChange} />
      <Toggle label="History enabled" description={description} value={false} onChange={onChange} />
      <SectionPanel title="Storage" helpTip={description}>Settings</SectionPanel></>);
    expect(screen.getByRole("textbox", { name: "History path" })).toHaveValue("/history");
    fireEvent.click(screen.getByRole("button", { name: "Help for History enabled" }));
    expect(screen.getByRole("dialog", { name: "Help for History enabled" })).toHaveFocus();
    expect(screen.getByRole("switch", { name: "History enabled" })).toHaveAttribute("aria-checked", "false");
    expect(onChange).not.toHaveBeenCalled();
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    await waitFor(() => expect(screen.getByRole("button", { name: "Help for History enabled" })).toHaveFocus());
    fireEvent.click(screen.getByRole("button", { name: "Help for Storage" }));
    expect(screen.getByRole("dialog", { name: "Help for Storage" })).toHaveAccessibleDescription(description);
  });
});
