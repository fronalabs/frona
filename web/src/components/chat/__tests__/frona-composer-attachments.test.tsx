import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useEffect } from "react";
import { AssistantRuntimeProvider, type AssistantRuntime } from "@assistant-ui/react";
import { beforeEach, expect, it, vi } from "vitest";
import { useChatRuntime, registerBackendAttachment } from "@/lib/use-chat-runtime";
import { FronaComposer } from "../frona-composer";

const mocks = vi.hoisted(() => ({ uploadFile: vi.fn(), sendMessage: vi.fn() }));
vi.mock("@/lib/api-client", () => ({
  api: { get: vi.fn(async () => ({ messages: [], has_more: false })) },
  uploadFile: mocks.uploadFile, sendMessage: mocks.sendMessage, cancelGeneration: vi.fn(),
}));
vi.mock("@/lib/format-time", () => ({ computeTimeMarkers: () => new Map(), useTimezone: () => "UTC" }));
vi.mock("next/navigation", () => ({ useRouter: () => ({ push: vi.fn() }) }));
vi.mock("../file-browser-modal", () => ({ FileBrowserModal: () => null }));
vi.mock("@/lib/use-commands", () => ({ useCommands: () => ({ commands: [{ name: "help", display_name: "help", description: "Show help" }, { name: "tester", display_name: "tester", description: "Test agent", argument_hint: "[prompt]" }], skills: [] }) }));

let runtime: AssistantRuntime;
function Harness({ onSend }: { onSend?: () => void }) {
  const chat = useChatRuntime({ chatId: "chat-1", agentId: "agent-1" });
  useEffect(() => { runtime = chat.runtime; }, [chat.runtime]);
  return <AssistantRuntimeProvider runtime={chat.runtime}><FronaComposer onSend={onSend} /></AssistantRuntimeProvider>;
}

beforeEach(() => {
  vi.clearAllMocks();
  HTMLElement.prototype.scrollIntoView = vi.fn();
});

it("opens an unrestricted native file picker", async () => {
  render(<Harness />);
  fireEvent.click(screen.getByTitle("Attach"));
  fireEvent.click(screen.getByText("Upload files"));
  const input = document.querySelector<HTMLInputElement>('input[type="file"]')!;
  expect(input).not.toHaveAttribute("accept");
  expect(input.multiple).toBe(true);
  fireEvent.change(input, { target: { files: [] } });
});

it.each([false, true])("uploads files selected after the attachment menu closes (standalone: %s)", async (standalone) => {
  const backend = { path: "uploads/picker", filename: "notes.txt", content_type: "text/plain", size_bytes: 1, owner: "user" };
  const secondBackend = { ...backend, path: "uploads/picker-second", filename: "second.txt" };
  mocks.uploadFile.mockResolvedValueOnce(backend).mockResolvedValueOnce(secondBackend);
  const onSend = standalone ? vi.fn() : undefined;
  render(<Harness onSend={onSend} />);
  fireEvent.click(screen.getByTitle("Attach"));
  fireEvent.click(screen.getByText("Upload files"));
  expect(screen.queryByRole("button", { name: "Upload files" })).not.toBeInTheDocument();
  const input = document.querySelector<HTMLInputElement>('input[type="file"]')!;
  const file = new File(["x"], "notes.txt", { type: "text/plain" });
  const secondFile = new File(["y"], "second.txt", { type: "text/plain" });
  fireEvent.change(input, { target: { files: [file, secondFile] } });
  await waitFor(() => expect(mocks.uploadFile).toHaveBeenCalledWith(file));
  expect(mocks.uploadFile).toHaveBeenCalledWith(secondFile);
  await waitFor(() => expect(screen.getByRole("button", { name: "Send message" })).toBeEnabled());
  expect(screen.getByText("notes.txt")).toBeVisible();
  expect(screen.getByText("second.txt")).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "Send message" }));
  await waitFor(() => {
    if (onSend) {
      expect(onSend).toHaveBeenCalledWith("", [backend, secondBackend]);
    } else {
      expect(mocks.sendMessage).toHaveBeenCalledWith("chat-1", { content: "", attachments: [backend, secondBackend] });
    }
  });
});

it.each([false, true])("keeps missing metadata recoverable in the composer (standalone: %s)", async (standalone) => {
  const onSend = standalone ? vi.fn() : undefined;
  render(<Harness onSend={onSend} />);
  await act(() => runtime.thread.composer.addAttachment({ id: "missing-ui", name: "notes.txt", content: [] }));
  act(() => runtime.thread.composer.setText("Keep my file"));
  fireEvent.submit(screen.getByRole("textbox").closest("form")!);
  expect(screen.getByRole("alert")).toHaveTextContent('"notes.txt" is no longer available');
  expect(runtime.thread.composer.getState().text).toBe("Keep my file");
  expect(mocks.sendMessage).not.toHaveBeenCalled();
  expect(onSend ?? mocks.sendMessage).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Remove attachment" }));
  const backend = { path: "uploads/recovered", filename: "notes.txt", content_type: "text/plain", size_bytes: 1, owner: "user" };
  registerBackendAttachment(backend.path, backend);
  await act(() => runtime.thread.composer.addAttachment({ id: backend.path, name: backend.filename, content: [] }));
  fireEvent.submit(screen.getByRole("textbox").closest("form")!);
  await waitFor(() => expect(onSend ?? mocks.sendMessage).toHaveBeenCalledOnce());
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it.each([false, true])("shows upload failures and blocks submission until reattached (standalone: %s)", async (standalone) => {
  let rejectUpload!: (error: Error) => void;
  mocks.uploadFile.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectUpload = reject; }));
  const onSend = standalone ? vi.fn() : undefined;
  render(<Harness onSend={onSend} />);
  let adding!: Promise<void>;
  await act(async () => {
    adding = runtime.thread.composer.addAttachment(new File(["x"], "notes.txt", { type: "text/plain" }));
  });
  expect(screen.getByText("Uploading…")).toBeInTheDocument();
  act(() => runtime.thread.composer.setText("Keep my draft"));
  fireEvent.submit(screen.getByRole("textbox").closest("form")!);
  expect(onSend ?? mocks.sendMessage).not.toHaveBeenCalled();
  await act(async () => { rejectUpload(new Error("Network unavailable")); await adding; });
  expect(screen.getAllByRole("alert").some((element) => element.textContent?.includes("Network unavailable"))).toBe(true);
  fireEvent.submit(screen.getByRole("textbox").closest("form")!);
  expect(onSend ?? mocks.sendMessage).not.toHaveBeenCalled();
  expect(runtime.thread.composer.getState().text).toBe("Keep my draft");
  fireEvent.click(screen.getByRole("button", { name: "Remove attachment" }));
  mocks.uploadFile.mockResolvedValueOnce({ path: "uploads/reattached", filename: "notes.txt", content_type: "text/plain", size_bytes: 1, owner: "user" });
  await act(() => runtime.thread.composer.addAttachment(new File(["x"], "notes.txt", { type: "text/plain" })));
  fireEvent.submit(screen.getByRole("textbox").closest("form")!);
  await waitFor(() => expect(onSend ?? mocks.sendMessage).toHaveBeenCalledOnce());
});

it("restores the draft when real Lexical Enter encounters missing metadata", async () => {
  render(<Harness />);
  await act(() => runtime.thread.composer.addAttachment({ id: "missing-enter", name: "notes.txt", content: [] }));
  act(() => runtime.thread.composer.setText("Keep my keyboard draft"));
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", code: "Enter", keyCode: 13 });
  expect(screen.getByRole("alert")).toHaveTextContent("notes.txt");
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("Keep my keyboard draft"));
  expect(runtime.thread.composer.getState().attachments).toHaveLength(1);
  expect(mocks.sendMessage).not.toHaveBeenCalled();
});

it("does not send an in-progress upload through real Lexical Enter", async () => {
  let resolveUpload!: (value: unknown) => void;
  mocks.uploadFile.mockImplementationOnce(() => new Promise((resolve) => { resolveUpload = resolve; }));
  render(<Harness />);
  let adding!: Promise<void>;
  await act(async () => {
    adding = runtime.thread.composer.addAttachment(new File(["x"], "notes.txt", { type: "text/plain" }));
  });
  act(() => runtime.thread.composer.setText("Wait for my upload"));
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", code: "Enter", keyCode: 13 });
  expect(screen.getByRole("alert")).toHaveTextContent("Wait for");
  expect(runtime.thread.composer.getState().attachments[0].status.type).toBe("running");
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("Wait for my upload"));
  expect(mocks.sendMessage).not.toHaveBeenCalled();
  const backend = { path: "uploads/eventually", filename: "notes.txt", content_type: "text/plain", size_bytes: 1, owner: "user" };
  await act(async () => { resolveUpload(backend); await adding; });
  expect(runtime.thread.composer.getState().attachments).toHaveLength(1);
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", code: "Enter", keyCode: 13 });
  await waitFor(() => expect(mocks.sendMessage).toHaveBeenCalledWith("chat-1", { content: "Wait for my upload", attachments: [backend] }));
});

it("preserves a failed upload's status when Enter is pressed, then permits reattachment", async () => {
  mocks.uploadFile.mockRejectedValueOnce(new Error("Network unavailable"));
  render(<Harness />);
  await act(() => runtime.thread.composer.addAttachment(new File(["x"], "failed.txt", { type: "text/plain" })));
  act(() => runtime.thread.composer.setText("Keep my file"));
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", code: "Enter", keyCode: 13 });
  expect(runtime.thread.composer.getState().attachments[0].status).toMatchObject({ type: "incomplete", message: expect.stringContaining("Network unavailable") });
  expect(runtime.thread.composer.getState().text).toBe("Keep my file");
  expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
  expect(mocks.sendMessage).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Remove attachment" }));
  const backend = { path: "uploads/keyboard-retry", filename: "failed.txt", content_type: "text/plain", size_bytes: 1, owner: "user" };
  mocks.uploadFile.mockResolvedValueOnce(backend);
  await act(() => runtime.thread.composer.addAttachment(new File(["x"], "failed.txt", { type: "text/plain" })));
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", code: "Enter", keyCode: 13 });
  await waitFor(() => expect(mocks.sendMessage).toHaveBeenCalledWith("chat-1", { content: "Keep my file", attachments: [backend] }));
});

it.each([[false, "/he", "/help"], [true, "/he", "/help"], [false, "@te", "@tester"], [true, "@te", "@tester"]] as const)("lets suggestions consume Enter before submitting (standalone: %s, query: %s)", async (standalone, query, selected) => {
  const onSend = standalone ? vi.fn() : undefined;
  render(<Harness onSend={onSend} />);
  act(() => runtime.thread.composer.setText(query));
  const editor = screen.getByRole("textbox");
  const selection = window.getSelection()!;
  const range = document.createRange();
  range.selectNodeContents(editor);
  range.collapse(false);
  selection.removeAllRanges();
  selection.addRange(range);
  fireEvent(document, new Event("selectionchange"));
  await screen.findByText(selected);
  fireEvent.keyDown(editor, { key: "Enter", code: "Enter", keyCode: 13 });
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe(`${selected} `));
  expect(onSend ?? mocks.sendMessage).not.toHaveBeenCalled();
});
