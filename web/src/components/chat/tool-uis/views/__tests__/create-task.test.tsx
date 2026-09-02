import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { CreateTaskView } from "../create-task";
import { mkProps } from "./helpers";

vi.mock("motion/react", () => ({
  AnimatePresence: ({ children }: { children: React.ReactNode }) => <>{children}</>,
  motion: new Proxy({}, {
    get: () => (props: Record<string, unknown>) => {
      const { children, ...rest } = props as { children?: React.ReactNode } & Record<string, unknown>;
      const filtered: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(rest)) {
        if (k === "initial" || k === "animate" || k === "exit" || k === "transition") continue;
        filtered[k] = v;
      }
      return <div {...filtered}>{children}</div>;
    },
  }),
}));

describe("CreateTaskView", () => {
  const baseArgs = {
    title: "Summarize the report",
    instruction: "Read the report and produce a 3-paragraph summary.",
  };

  it("renders Create Task title and task title as subtitle", () => {
    render(
      <CreateTaskView
        {...mkProps({
          toolName: "create_task",
          args: baseArgs,
          result: JSON.stringify({
            task_id: "t-1",
            target_agent: "Assistant",
            run_at: null,
            message: "Task created and running.",
          }),
        })}
      />,
    );
    expect(screen.getByText("Create Task")).toBeInTheDocument();
    expect(screen.getByText("— Summarize the report")).toBeInTheDocument();
  });

  it("omits the schedule block when there's no run_at (immediate task)", () => {
    render(
      <CreateTaskView
        {...mkProps({
          toolName: "create_task",
          args: baseArgs,
          result: JSON.stringify({
            task_id: "t-1",
            target_agent: "Assistant",
            run_at: null,
            message: "running",
          }),
        })}
      />,
    );
    expect(screen.queryByText(/Scheduled for/)).not.toBeInTheDocument();
    expect(screen.queryByText(/Running now/)).not.toBeInTheDocument();
  });

  it("shows 'Scheduled for ...' when run_at is set", () => {
    render(
      <CreateTaskView
        {...mkProps({
          toolName: "create_task",
          args: { ...baseArgs, timezone: "America/Los_Angeles" },
          result: JSON.stringify({
            task_id: "t-1",
            target_agent: "Assistant",
            run_at: "2026-06-14T03:50:00+00:00",
            message: "deferred",
          }),
        })}
      />,
    );
    const block = screen.getByText(/Scheduled for/);
    expect(block.textContent).toMatch(/Jun\b/);
    expect(block.textContent).toMatch(/PDT|PST/);
  });

  it("renders the instruction text", () => {
    render(
      <CreateTaskView
        {...mkProps({ toolName: "create_task", args: baseArgs, result: undefined })}
      />,
    );
    expect(screen.getByText("Instruction")).toBeInTheDocument();
    expect(
      screen.getByText("Read the report and produce a 3-paragraph summary."),
    ).toBeInTheDocument();
  });

  it("shows the target agent when delegating", () => {
    render(
      <CreateTaskView
        {...mkProps({
          toolName: "create_task",
          args: { ...baseArgs, target_agent: "Receptionist" },
          result: JSON.stringify({
            task_id: "t-1",
            target_agent: "Receptionist",
            run_at: null,
          }),
        })}
      />,
    );
    expect(screen.getByText("Receptionist")).toBeInTheDocument();
  });

  it("shows 'Resume on result' when process_result is true", () => {
    render(
      <CreateTaskView
        {...mkProps({
          toolName: "create_task",
          args: { ...baseArgs, process_result: true },
          result: undefined,
        })}
      />,
    );
    expect(screen.getByText(/Resume on result/)).toBeInTheDocument();
  });

  it("falls back to args.run_at when result hasn't arrived yet", () => {
    render(
      <CreateTaskView
        {...mkProps({
          toolName: "create_task",
          args: { ...baseArgs, run_at: "2026-06-14T03:50:00+00:00", timezone: "UTC" },
          result: undefined,
        })}
      />,
    );
    expect(screen.getByText(/Scheduled for/)).toBeInTheDocument();
  });
});
