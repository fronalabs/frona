import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  push: vi.fn(),
  refresh: vi.fn(async () => {}),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: mocks.push }),
}));

vi.mock("@/lib/activity-context", () => ({
  useActivity: () => ({
    snapshot: {
      executions: [
        {
          id: "execution-1",
          title: "Research prices",
          kind: "task",
          status: "running",
          action: "Searching the web",
          source: { type: "task", id: "task-1" },
          startedAt: "2026-09-03T00:00:00Z",
          canCancel: true,
        },
      ],
    },
    stale: false,
    refresh: mocks.refresh,
  }),
}));

import { ActivityIndicator } from "../activity-indicator";

describe("ActivityIndicator", () => {
  it("joins the open trigger to the menu with the standard open-menu colors", () => {
    render(<ActivityIndicator />);

    const trigger = screen.getByRole("button", { name: "1 active execution" });
    expect(trigger).toHaveClass("rounded-full", "bg-info-bg", "text-info-text");

    fireEvent.click(trigger);

    expect(screen.queryByText("1 currently active")).not.toBeInTheDocument();
    expect(trigger).toHaveClass(
      "rounded-t-xl",
      "rounded-b-none",
      "border-b-0",
      "border-border",
      "bg-surface-secondary",
      "text-text-primary",
    );
    expect(trigger).not.toHaveClass("rounded-full", "bg-info-bg", "text-info-text");
    expect(trigger.parentElement?.querySelector("div[aria-hidden='true']")).toHaveClass(
      "top-[calc(100%-1px)]",
      "z-[60]",
    );

    const heading = screen.getByRole("heading", { name: "Activity" });
    expect(heading).toHaveClass("text-sm", "font-medium", "text-text-secondary");
    const header = heading.parentElement?.parentElement;
    expect(header).toHaveClass("px-4", "py-2", "border-b", "shrink-0");
    expect(header).not.toHaveClass("py-3");

    const panel = header?.parentElement;
    expect(panel).toHaveClass("top-full", "rounded-xl", "rounded-tr-none", "shadow-lg");
    expect(panel).not.toHaveClass("mt-2");

    const list = screen.getByText("Research prices").closest("button")?.parentElement;
    expect(list).toHaveClass("px-4", "py-2");
    expect(list).not.toHaveClass("rounded-lg", "px-3", "py-2.5");
    expect(list?.parentElement).toHaveClass("pb-1");
    expect(list?.parentElement).not.toHaveClass("p-2");
  });
});
