import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { CreateTaskView } from "../create-task";
import { DeleteTaskView } from "../delete-task";
import { HeartbeatView } from "../heartbeat";
import { ProduceFileView } from "../produce-file";
import { RecurringTaskView } from "../recurring-task";
import { SafeToolView, ToolViewFallback } from "../safe-tool-view";
import type { ToolView } from "../types";
import { mkProps } from "./helpers";

vi.mock("motion/react", () => ({
  AnimatePresence: ({ children }: { children: React.ReactNode }) => <>{children}</>,
  motion: new Proxy(
    {},
    {
      get: () => (props: Record<string, unknown>) => {
        const { children, ...rest } = props as { children?: React.ReactNode } & Record<
          string,
          unknown
        >;
        const filtered: Record<string, unknown> = {};
        for (const [key, value] of Object.entries(rest)) {
          if (["initial", "animate", "exit", "transition"].includes(key)) continue;
          filtered[key] = value;
        }
        return <div {...filtered}>{children}</div>;
      },
    },
  ),
}));

vi.mock("@/components/ui/code-block", () => ({
  CodeBlock: ({ code }: { code: string }) => <pre>{code}</pre>,
}));

describe("SafeToolView", () => {
  it("lets a custom view reject an unsupported persisted result", () => {
    const RejectingView: ToolView = () => <ToolViewFallback />;

    render(
      <SafeToolView
        view={RejectingView}
        {...mkProps({
          toolName: "memory_search",
          argsText: JSON.stringify({ query: "Mina" }),
          result: "legacy result",
        })}
      />,
    );

    expect(screen.getByText("Memory Search")).toBeInTheDocument();
    expect(screen.getByText("Result:")).toBeInTheDocument();
    expect(screen.getByText("legacy result")).toBeInTheDocument();
  });

  it("catches a custom view render error and uses the generic view", () => {
    const error = new Error("broken custom view");
    const BrokenView: ToolView = () => {
      throw error;
    };
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);

    render(
      <SafeToolView
        view={BrokenView}
        {...mkProps({ toolName: "demo_tool", result: "preserved result" })}
      />,
    );

    expect(screen.getByText("Demo Tool")).toBeInTheDocument();
    expect(screen.getByText("preserved result")).toBeInTheDocument();
    expect(consoleError).toHaveBeenCalled();
    consoleError.mockRestore();
  });

  it.each<[string, ToolView]>([
    ["create_task", CreateTaskView],
    ["create_recurring_task", RecurringTaskView],
    ["delete_task", DeleteTaskView],
    ["set_heartbeat", HeartbeatView],
    ["produce_file", ProduceFileView],
  ])(
    "uses the generic view when %s receives an unsupported result",
    (toolName, view) => {
      render(
        <SafeToolView
          view={view}
          {...mkProps({ toolName, result: JSON.stringify({ old_shape: true }) })}
        />,
      );

      expect(screen.getByText("Result:")).toBeInTheDocument();
      expect(screen.getByText(JSON.stringify({ old_shape: true }))).toBeInTheDocument();
    },
  );

  it("retries the custom view when streamed tool data changes", async () => {
    const ConditionalView: ToolView = ({ result }) => {
      if (result === "bad") throw new Error("bad result");
      return <div>Custom view recovered</div>;
    };
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const initialProps = mkProps({ toolName: "demo_tool", result: "bad" });
    const { rerender } = render(<SafeToolView view={ConditionalView} {...initialProps} />);

    expect(screen.getByText("bad")).toBeInTheDocument();

    rerender(
      <SafeToolView
        view={ConditionalView}
        {...mkProps({ ...initialProps, result: "good" })}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("Custom view recovered")).toBeInTheDocument();
    });
    consoleError.mockRestore();
  });
});
