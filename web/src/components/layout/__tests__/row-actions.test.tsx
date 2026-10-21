import React from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { AgentDropdown } from "../agent-dropdown";
import { ChatActions } from "@/components/nav/chat-actions";
import { TaskActions } from "@/components/nav/task-actions";

const mocks = vi.hoisted(() => ({ push: vi.fn(), deleteAgent: vi.fn() }));
vi.mock("next/navigation", () => ({ useRouter: () => ({ push: mocks.push }) }));
vi.mock("@/lib/navigation-context", () => ({ useNavigation: () => ({
  agents: [{ id: "agent-1", name: "My agent", is_shared: false }], deleteAgent: mocks.deleteAgent,
}) }));

beforeEach(() => {
  vi.clearAllMocks();
  mocks.deleteAgent.mockResolvedValue(undefined);
  vi.spyOn(window, "confirm").mockReturnValue(true);
});
afterEach(() => vi.restoreAllMocks());

it("names agent Configure/Delete actions and routes Configure without selecting a chat", async () => {
  render(<AgentDropdown />);
  fireEvent.click(screen.getByRole("button", { name: "Agents" }));
  const configure = screen.getByRole("button", { name: "Configure My Agent" });
  expect(configure).toHaveClass("row-action", "group-focus-within:opacity-100");
  expect(screen.getByRole("button", { name: "Delete My Agent" })).toHaveClass("row-action");
  fireEvent.click(configure);
  expect(mocks.push).toHaveBeenCalledExactlyOnceWith("/agents?id=agent-1");
  fireEvent.click(screen.getByRole("button", { name: "Agents" }));
  fireEvent.click(screen.getByRole("button", { name: "Delete My Agent" }));
  await waitFor(() => expect(mocks.deleteAgent).toHaveBeenCalledExactlyOnceWith("agent-1"));
});

it("closes the agent picker on Escape and restores trigger focus", async () => {
  render(<AgentDropdown />);
  const opener = screen.getByRole("button", { name: "Agents" });
  fireEvent.click(opener);
  fireEvent.keyDown(screen.getByRole("button", { name: "Configure My Agent" }), { key: "Escape" });
  expect(screen.queryByRole("button", { name: "Configure My Agent" })).not.toBeInTheDocument();
  expect(opener).toHaveFocus();
});

it("exposes named chat and task overflow buttons and preserves their actions", () => {
  const onArchive = vi.fn();
  const onCancel = vi.fn();
  render(<><ChatActions isArchived={false} onArchive={onArchive} onUnarchive={vi.fn()} onDelete={vi.fn()} /><TaskActions canCancel canDelete onCancel={onCancel} onDelete={vi.fn()} /></>);
  fireEvent.click(screen.getByRole("button", { name: "Chat actions" }));
  fireEvent.click(screen.getByRole("button", { name: "Archive" }));
  expect(onArchive).toHaveBeenCalledOnce();
  fireEvent.click(screen.getByRole("button", { name: "Task actions" }));
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(onCancel).toHaveBeenCalledOnce();
});
