import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ToolCallMessagePart } from "@assistant-ui/react";
import { apiFetch } from "@/lib/api-client";
import { ToolTimelineProvider } from "../../tool-timeline-context";
import { DefaultToolCallUI } from "../../default-tool-call-ui";
import { BrowserView } from "../browser";
import { pickView } from "..";
import { mkProps } from "./helpers";

const runtime = vi.hoisted(() => ({
  message: {
    role: "assistant", isLast: true, status: { type: "running" },
    content: [] as ToolCallMessagePart[],
  },
}));
vi.mock("@assistant-ui/react", () => ({
  useAuiState: (selector: (state: typeof runtime) => unknown) => selector(runtime),
}));
vi.mock("@assistant-ui/core/react", () => ({ useThreadIsRunning: () => true }));
vi.mock("@/lib/api-client", () => ({ apiFetch: vi.fn() }));
vi.mock("@/components/ui/code-block", () => ({ CodeBlock: () => null }));

function call(id: string, name: string, result?: string): ToolCallMessagePart {
  return { type: "tool-call", toolCallId: id, toolName: name, args: {}, argsText: "{}", result };
}

function Timeline() {
  return (
    <ToolTimelineProvider>
      {runtime.message.content.map((part) => (
        <DefaultToolCallUI key={part.toolCallId} {...mkProps(part)} />
      ))}
    </ToolTimelineProvider>
  );
}

function imageResponse() {
  return { ok: true, status: 200, blob: async () => new Blob(["png"], { type: "image/png" }) } as Response;
}

beforeEach(() => {
  vi.mocked(apiFetch).mockReset().mockResolvedValue(imageResponse());
  let image = 0;
  vi.stubGlobal("URL", Object.assign(URL, {
    createObjectURL: vi.fn(() => `blob:snapshot-${++image}`),
    revokeObjectURL: vi.fn(),
  }));
  runtime.message.content = [];
});

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

describe("shared browser widget", () => {
  it("keeps a usable snapshot when navigation reports iframe warnings", async () => {
    const warning = "Iframe did not become ready before the navigation timeout: https://ui.example.com";
    runtime.message.content = [call("nav", "browser_navigate", JSON.stringify({ warnings: [warning] }))];
    const { rerender } = render(<Timeline />);
    await screen.findByRole("img");
    expect(screen.getByText("Page ready with warnings")).toBeInTheDocument();
    expect(screen.getByText(warning)).toBeInTheDocument();
    expect(screen.queryByText("Browser action failed")).toBeNull();

    runtime.message.content = [...runtime.message.content, call("extract", "browser_extract", "Ready")];
    rerender(<Timeline />);
    await waitFor(() => expect(apiFetch).toHaveBeenCalledTimes(2));
    expect(screen.getByText(warning)).toBeInTheDocument();

    runtime.message.content = [...runtime.message.content, call("nav2", "browser_navigate", JSON.stringify({ url: "https://example.com" }))];
    rerender(<Timeline />);
    await waitFor(() => expect(apiFetch).toHaveBeenCalledTimes(3));
    expect(screen.queryByText("Page ready with warnings")).toBeNull();
  });

  it("uses the same custom view for every browser operation", () => {
    for (const name of ["navigate", "click", "scroll", "evaluate", "snapshot", "screenshot", "close", "switch_tab"]) {
      expect(pickView(`browser_${name}`)).toBe(BrowserView);
    }
    expect(pickView("web_fetch")).not.toBe(BrowserView);
  });

  it("keeps one mounted image and refreshes only as browser calls complete", async () => {
    runtime.message.content = [call("nav", "browser_navigate")];
    const { rerender, unmount } = render(<Timeline />);
    expect(screen.getAllByText("Browser")).toHaveLength(1);
    expect(apiFetch).not.toHaveBeenCalled();

    runtime.message.content = [call("nav", "browser_navigate", "loaded")];
    rerender(<Timeline />);
    const image = await screen.findByRole("img", { name: "Current browser page" });
    expect(image).toHaveAttribute("src", "blob:snapshot-1");

    runtime.message.content = [...runtime.message.content, call("click", "browser_click")];
    rerender(<Timeline />);
    expect(screen.getAllByText("Browser")).toHaveLength(1);
    expect(screen.getByRole("img")).toBe(image);
    expect(apiFetch).toHaveBeenCalledTimes(1);

    runtime.message.content = [runtime.message.content[0], call("click", "browser_click", "")];
    rerender(<Timeline />);
    await waitFor(() => expect(image).toHaveAttribute("src", "blob:snapshot-2"));
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:snapshot-1");

    runtime.message.content = [...runtime.message.content, call("shell", "shell", "done")];
    rerender(<Timeline />);
    expect(apiFetch).toHaveBeenCalledTimes(2);
    unmount();
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:snapshot-2");
  });

  it("ignores stale responses and retains the previous image when capture fails", async () => {
    runtime.message.content = [call("nav", "browser_navigate", "done")];
    const { rerender } = render(<Timeline />);
    const image = await screen.findByRole("img");
    let finish!: (response: Response) => void;
    vi.mocked(apiFetch).mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    runtime.message.content.push(call("click", "browser_click", "done"));
    runtime.message.content = [...runtime.message.content];
    rerender(<Timeline />);

    runtime.message.content = [...runtime.message.content, call("scroll", "browser_scroll", "done")];
    rerender(<Timeline />);
    await waitFor(() => expect(image).toHaveAttribute("src", "blob:snapshot-2"));
    await act(async () => finish(imageResponse()));
    expect(image).toHaveAttribute("src", "blob:snapshot-2");
    expect(URL.createObjectURL).toHaveBeenCalledTimes(2);

    vi.mocked(apiFetch).mockRejectedValueOnce(new Error("offline"));
    runtime.message.content = [...runtime.message.content, call("wait", "browser_wait", "done")];
    rerender(<Timeline />);
    await screen.findByText(/Snapshot unavailable/);
    expect(image).toHaveAttribute("src", "blob:snapshot-2");
  });

  it("clears the snapshot on close and refreshes after a later navigation", async () => {
    runtime.message.content = [call("nav", "browser_navigate", "done")];
    const { rerender } = render(<Timeline />);
    await screen.findByRole("img");
    runtime.message.content = [...runtime.message.content, call("close", "browser_close", "Browser closed.")];
    rerender(<Timeline />);
    await screen.findByText("Browser closed");
    expect(screen.queryByRole("img")).toBeNull();
    expect(apiFetch).toHaveBeenCalledTimes(1);
    runtime.message.content = [...runtime.message.content, call("nav2", "browser_navigate", "done")];
    rerender(<Timeline />);
    await screen.findByRole("img");
    expect(apiFetch).toHaveBeenCalledTimes(2);
  });

  it("keeps the browser card visible when other tools fill the timeline", async () => {
    runtime.message.content = [call("nav", "browser_navigate", "done"),
      ...Array.from({ length: 8 }, (_, i) => call(`other-${i}`, "web_search", "done")),
      call("click", "browser_click", "done")];
    render(<Timeline />);
    expect(screen.getAllByText("Browser")).toHaveLength(1);
    await screen.findByRole("img");
    expect(apiFetch).toHaveBeenCalledTimes(1);
  });

  it("shows failed actions and can refresh again on the next completion", async () => {
    const failed = call("click", "browser_click", "Element not found");
    runtime.message.content = [{ ...failed, args: { isError: true } }];
    vi.mocked(apiFetch).mockResolvedValueOnce({ status: 204 } as Response);
    const { rerender } = render(<Timeline />);
    await screen.findByText("No active browser page");
    expect(screen.getAllByText("Element not found").length).toBeGreaterThan(0);
    runtime.message.content = [...runtime.message.content, call("nav", "browser_navigate", "done")];
    rerender(<Timeline />);
    await screen.findByRole("img");
  });
});
