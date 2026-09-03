import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Execution } from "@/lib/types";

const mocks = vi.hoisted(() => ({
  refresh: vi.fn(async () => {}),
  push: vi.fn(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: mocks.push }),
}));

vi.mock("@/lib/activity-context", () => ({
  useActivity: () => ({ refresh: mocks.refresh }),
}));

import {
  ChatActivityDrawer,
  ChatActivityTab,
  executionsForChat,
  useChatExecutions,
} from "../chat-activity-drawer";

function execution(overrides: Partial<Execution> = {}): Execution {
  return {
    id: "execution-1",
    title: "Research competitors",
    kind: "task",
    status: "running",
    action: "Searching the web",
    source: { type: "task", id: "task-1" },
    relatedChatIds: ["chat-1"],
    startedAt: "2026-09-02T12:00:00Z",
    canCancel: true,
    ...overrides,
  };
}

function ChatExecutionCount() {
  return <span>{useChatExecutions().length}</span>;
}

describe("chat activity drawer", () => {
  it("renders an empty activity list while a pending chat has no provider", () => {
    render(<ChatExecutionCount />);

    expect(screen.getByText("0")).toBeInTheDocument();
  });

  it("shows only background executions related to the current chat", () => {
    const executions = [
      execution(),
      execution({ id: "other", relatedChatIds: ["chat-2"] }),
      execution({ id: "foreground", kind: "inference" }),
      execution({ id: "global", kind: "memory", relatedChatIds: undefined }),
    ];

    expect(executionsForChat(executions, "chat-1").map((item) => item.id)).toEqual([
      "execution-1",
    ]);
  });

  it("expands from a temporary composer tab", () => {
    const onExpand = vi.fn();
    render(<ChatActivityTab executions={[execution()]} onExpand={onExpand} />);

    const tab = screen.getByRole("button", { name: "Expand 1 background activity" });
    expect(tab).toHaveClass("max-w-[80%]");
    expect(tab).not.toHaveClass("w-4/5");
    expect(screen.queryByText("Searching the web")).not.toBeInTheDocument();
    fireEvent.click(tab);

    expect(onExpand).toHaveBeenCalledOnce();
  });

  it("renders the active work and collapses on request", () => {
    const onCollapse = vi.fn();
    render(
      <ChatActivityDrawer executions={[execution()]} onCollapse={onCollapse} />,
    );

    expect(screen.getByText("Research competitors")).toBeInTheDocument();
    expect(screen.getByText(/Searching the web/)).toBeInTheDocument();

    fireEvent.click(
      screen.getByRole("button", { name: "Collapse background activity" }),
    );
    expect(onCollapse).toHaveBeenCalledOnce();
  });

  it("renders nothing after the authoritative snapshot becomes empty", () => {
    const { rerender } = render(
      <ChatActivityTab executions={[execution()]} onExpand={() => {}} />,
    );
    expect(screen.getByText("Research competitors")).toBeInTheDocument();

    rerender(<ChatActivityTab executions={[]} onExpand={() => {}} />);

    expect(screen.queryByText("Research competitors")).not.toBeInTheDocument();
  });
});
