import React, { useEffect } from "react";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AssistantRuntimeProvider, ThreadPrimitive, useExternalStoreRuntime, useMessagePartText, type AssistantRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import { FronaAssistantMessage } from "../frona-assistant-message";
import { FronaUserMessage } from "../frona-user-message";
import { FronaComposer } from "../frona-composer";
import { DefaultToolCallUI } from "../tool-uis/default-tool-call-ui";
import { mkProps } from "../tool-uis/views/__tests__/helpers";

const faults = vi.hoisted(() => ({ message: new Error("injected Markdown render failure"), attempts: vi.fn(), unrelated: false }));
vi.mock("@assistant-ui/react-markdown", async importOriginal => {
  const actual = await importOriginal<typeof import("@assistant-ui/react-markdown")>();
  return { ...actual, MarkdownTextPrimitive: (props: React.ComponentProps<typeof actual.MarkdownTextPrimitive>) => {
    const { text } = useMessagePartText();
    faults.attempts(text);
    if (text.includes("BROKEN")) throw faults.message;
    return <actual.MarkdownTextPrimitive {...props} />;
  } };
});
vi.mock("react-markdown", async importOriginal => {
  const actual = await importOriginal<typeof import("react-markdown")>();
  return { ...actual, default: (props: React.ComponentProps<typeof actual.default>) => {
    if (props.children?.includes("BROKEN")) throw faults.message;
    return <actual.default {...props} />;
  } };
});
vi.mock("@/lib/session-context", () => ({ useSession: () => ({ agentId: "agent", activeTaskId: null }) }));
vi.mock("@/lib/navigation-context", () => ({ useNavigation: () => {
  if (faults.unrelated) throw new Error("unrelated navigation error");
  return { agents: [] };
} }));
vi.mock("@/lib/format-time", () => ({ useTimezone: () => "UTC", formatFullTimestamp: () => "Now", formatTime: () => "Now" }));
vi.mock("next/navigation", () => ({ useRouter: () => ({ push: vi.fn() }) }));
vi.mock("../file-browser-modal", () => ({ FileBrowserModal: () => null }));
vi.mock("@/lib/use-commands", () => ({ useCommands: () => ({ commands: [], skills: [] }) }));

let runtime: AssistantRuntime;
const onNew = vi.fn(async () => {});
function Chat({ text, role = "assistant" }: { text: string; role?: "assistant" | "user" }) {
  const messages: ThreadMessageLike[] = [
    { id: "earlier", role: "assistant", content: [{ type: "text", text: "**Earlier answer**" }] },
    { id: "streaming", role, content: [{ type: "text", text }] },
  ];
  const chat = useExternalStoreRuntime({ messages, isRunning: true, onNew, convertMessage: message => message });
  useEffect(() => { runtime = chat; }, [chat]);
  return <AssistantRuntimeProvider runtime={chat}>
    <ThreadPrimitive.Root>
      <ThreadPrimitive.Messages components={{ AssistantMessage: FronaAssistantMessage, UserMessage: FronaUserMessage }} />
      <FronaComposer />
    </ThreadPrimitive.Root>
  </AssistantRuntimeProvider>;
}

beforeEach(() => {
  vi.clearAllMocks();
  faults.unrelated = false;
  vi.spyOn(console, "error").mockImplementation(() => {});
  HTMLElement.prototype.scrollIntoView = vi.fn();
  vi.spyOn(window, "scrollTo").mockImplementation(() => {});
});
afterEach(() => vi.restoreAllMocks());

describe("Markdown render recovery", () => {
  it.each(["assistant", "user"] as const)("contains a streamed %s text failure and preserves the runtime, other messages and draft", async role => {
    const view = render(<Chat text="**Starting**" role={role} />);
    expect(await screen.findByText("Starting", { selector: "strong" })).toBeInTheDocument();
    const originalRuntime = runtime;
    act(() => runtime.thread.composer.setText("Keep this unsent draft"));
    await act(() => runtime.thread.composer.addAttachment({ id: "staged", name: "draft.txt", content: [] }));
    const raw = 'BROKEN\n<img src=x onerror="alert(1)">\n**unfinished';
    view.rerender(<Chat text={raw} role={role} />);
    await waitFor(() => expect(screen.getByText(/BROKEN/).textContent).toBe(raw));
    expect(screen.getByText(/BROKEN/)).toHaveClass("whitespace-pre-wrap", "break-words");
    expect(view.container.querySelector('img[src="x"]')).toBeNull();
    expect(screen.getByText("Earlier answer", { selector: "strong" })).toBeInTheDocument();
    expect(runtime).toBe(originalRuntime);
    expect(runtime.thread.getState().messages.map(message => message.id)).toEqual(expect.arrayContaining(["earlier", "streaming"]));
    expect(runtime.thread.composer.getState().text).toBe("Keep this unsent draft");
    expect(runtime.thread.composer.getState().attachments).toHaveLength(1);
    expect(console.error).toHaveBeenCalledWith("Markdown rendering failed; displaying plain text.", faults.message, expect.objectContaining({ componentStack: expect.any(String) }));

    const attempts = faults.attempts.mock.calls.filter(([text]) => text === raw).length;
    view.rerender(<Chat text={raw} role={role} />);
    act(() => runtime.thread.composer.setText("Draft still editable"));
    await waitFor(() => expect(screen.getByRole("textbox")).toHaveTextContent("Draft still editable"));
    expect(faults.attempts.mock.calls.filter(([text]) => text === raw)).toHaveLength(attempts);
    fireEvent.click(screen.getByRole("button", { name: "Remove attachment" }));
    await waitFor(() => expect(runtime.thread.composer.getState().attachments).toHaveLength(0));

    view.rerender(<Chat text="**Recovered answer**" role={role} />);
    expect(await screen.findByText("Recovered answer", { selector: "strong" })).toBeInTheDocument();
    expect(screen.queryByText(/BROKEN/)).not.toBeInTheDocument();
    expect(runtime).toBe(originalRuntime);
    expect(runtime.thread.composer.getState().text).toBe("Draft still editable");
    expect(runtime.thread.getState().messages.map(message => message.id)).toEqual(expect.arrayContaining(["earlier", "streaming"]));
    expect(onNew).not.toHaveBeenCalled();
  });

  it("updates the readable fallback on repeated failures, then recovers on a later chunk", async () => {
    const view = render(<Chat text="BROKEN first" />);
    expect(await screen.findByText("BROKEN first")).toBeInTheDocument();
    view.rerender(<Chat text="BROKEN second" />);
    expect(await screen.findByText("BROKEN second")).toBeInTheDocument();
    expect(screen.queryByText("BROKEN first")).not.toBeInTheDocument();
    view.rerender(<Chat text="**Eventually valid**" />);
    expect(await screen.findByText("Eventually valid", { selector: "strong" })).toBeInTheDocument();
  });

  it("contains tool narration Markdown without hiding or duplicating the existing tool view", async () => {
    const raw = 'BROKEN <script>alert("x")</script>';
    const props = mkProps({ toolName: "demo_tool", args: { turnText: raw }, result: "Retained tool result" });
    const view = render(<DefaultToolCallUI {...props} />);
    expect(screen.getByText(raw).textContent).toBe(raw);
    expect(view.container.querySelector("script")).toBeNull();
    fireEvent.click(screen.getByText("Demo Tool"));
    expect(screen.getByText("Retained tool result")).toBeInTheDocument();
    view.rerender(<DefaultToolCallUI {...props} args={{ turnText: "**Narration recovered**" }} />);
    expect(await screen.findByText("Narration recovered", { selector: "strong" })).toBeInTheDocument();
    expect(screen.getAllByText("Retained tool result")).toHaveLength(1);
  });

  it("lets unrelated message failures escape to the route boundary", () => {
    faults.unrelated = true;
    expect(() => render(<Chat text="Valid text" />)).toThrow("unrelated navigation error");
  });
});
