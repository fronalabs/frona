import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useEffect } from "react";
import { AssistantRuntimeProvider, type AssistantRuntime } from "@assistant-ui/react";
import { $isDirectiveNode } from "@assistant-ui/react-lexical";
import { $createNodeSelection, $getRoot, $isElementNode, $setSelection, getNearestEditorFromDOMNode, UNDO_COMMAND } from "lexical";
import { beforeEach, expect, it, vi } from "vitest";
import { useChatRuntime } from "@/lib/use-chat-runtime";
import { FronaComposer } from "../frona-composer";

// Enable the browser's real beforeinput path in Lexical's jsdom environment.
vi.hoisted(() => Object.defineProperty(InputEvent.prototype, "getTargetRanges", { configurable: true, value: () => [] }));
const mocks = vi.hoisted(() => ({ uploadFile: vi.fn(), sendMessage: vi.fn() }));
vi.mock("@/lib/api-client", () => ({
  api: { get: vi.fn(async () => ({ messages: [], has_more: false })) },
  uploadFile: mocks.uploadFile, sendMessage: mocks.sendMessage, cancelGeneration: vi.fn(),
}));
vi.mock("@/lib/format-time", () => ({ computeTimeMarkers: () => new Map(), useTimezone: () => "UTC" }));
vi.mock("next/navigation", () => ({ useRouter: () => ({ push: vi.fn() }) }));
vi.mock("../file-browser-modal", () => ({ FileBrowserModal: () => null }));
vi.mock("@/lib/use-commands", () => ({ useCommands: () => ({ commands: [{ name: "tester", display_name: "tester", description: "Test agent", argument_hint: "[prompt]" }], skills: [] }) }));

let runtime: AssistantRuntime;
function Harness() {
  const chat = useChatRuntime({ chatId: "chat-1", agentId: "agent-1" });
  useEffect(() => { runtime = chat.runtime; }, [chat.runtime]);
  return <AssistantRuntimeProvider runtime={chat.runtime}><FronaComposer /></AssistantRuntimeProvider>;
}

function transfer(data: Record<string, string | undefined> = {}, files: File[] = []) {
  return { getData: (type: string) => data[type] ?? "", files, items: [] };
}

function paste(data: ReturnType<typeof transfer>) {
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", { value: data });
  fireEvent(screen.getByRole("textbox"), event);
  return event;
}

function beforePaste(data: string | null, dataTransfer?: ReturnType<typeof transfer>) {
  const event = new InputEvent("beforeinput", { inputType: "insertFromPaste", data, bubbles: true, cancelable: true });
  if (dataTransfer) Object.defineProperty(event, "dataTransfer", { value: dataTransfer });
  fireEvent(screen.getByRole("textbox"), event);
  return event;
}

async function selectText(start: number, end: number) {
  await waitFor(() => expect(screen.getByRole("textbox").querySelector("[data-lexical-text]")).not.toBeNull());
  const text = screen.getByRole("textbox").querySelector("[data-lexical-text]")!.firstChild!;
  const range = document.createRange();
  range.setStart(text, start);
  range.setEnd(text, end);
  window.getSelection()!.removeAllRanges();
  window.getSelection()!.addRange(range);
  await act(async () => { fireEvent(document, new Event("selectionchange")); });
}

beforeEach(() => {
  vi.clearAllMocks();
  HTMLElement.prototype.scrollIntoView = vi.fn();
  Range.prototype.getBoundingClientRect = () => new DOMRect();
  mocks.uploadFile.mockImplementation(async (file: File) => ({ path: `uploads/${file.name}`, filename: file.name, content_type: file.type, size_bytes: file.size, owner: "user" }));
});

it.each([
  { name: "plain text wins over HTML", data: { "text/plain": "  literal <b>text</b>  ", "text/html": "<b>different</b>" }, expected: "  literal <b>text</b>  " },
  { name: "Unicode joiners, emoji and line endings", data: { "text/plain": "می\u200cروم 👩\u200d💻\r\nक्\u200dष\rnext\n" }, expected: "می\u200cروم 👩\u200d💻\nक्\u200dष\nnext\n" },
  { name: "inert HTML with block/BR boundaries", data: { "text/html": "<p>Hello <b>world</b></p><div>next<br>line<script>bad()</script><style>bad{}</style></div>" }, expected: "Hello world\nnext\nline" },
  { name: "HTML preserves explicit whitespace", data: { "text/html": "<pre>  a\n\n</pre>" }, expected: "  a\n\n" },
  { name: "HTML table cell boundaries", data: { "text/html": "<table><tr><td>A</td><td>B</td></tr></table>" }, expected: "A\tB" },
  { name: "HTML table headers and empty cells", data: { "text/html": "<table><tr><th>A</th><th></th><th>B</th><th></th></tr><tr><td></td><td>C</td><td></td><td></td></tr></table>" }, expected: "A\t\tB\t\n\tC\t\t" },
  { name: "URL-only clipboard", data: { "text/uri-list": "# Source\r\nhttps://example.test/one\r\nhttps://example.test/two" }, expected: "https://example.test/one\nhttps://example.test/two" },
])("normalizes $name through the real mounted composer", async ({ data, expected }) => {
  render(<Harness />);
  paste(transfer(data));
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe(expected));
  expect(screen.getByRole("textbox").querySelector("script, style, b")).toBeNull();
  expect(mocks.sendMessage).not.toHaveBeenCalled();
});

it("inserts Android beforeinput data at the selected text and gives paste its own undo step", async () => {
  render(<Harness />);
  act(() => runtime.thread.composer.setText("hello old world"));
  await selectText(6, 9);
  beforePaste("new\r\n👨‍👩‍👧‍👦");
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("hello new\n👨‍👩‍👧‍👦 world"));
  act(() => getNearestEditorFromDOMNode(screen.getByRole("textbox"))!.dispatchCommand(UNDO_COMMAND, undefined));
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("hello old world"));
});

it.each(["paste", "beforeinput"])("pairs opposite event phases without suppressing rapid repeated %s operations", async (first) => {
  render(<Harness />);
  const sendFirst = () => first === "paste" ? paste(transfer({ "text/plain": "same" })) : beforePaste("same");
  const sendPair = () => first === "paste" ? beforePaste("same") : paste(transfer({ "text/plain": "same" }));
  act(() => { sendFirst(); sendPair(); sendFirst(); sendPair(); });
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("samesame"));
  act(() => { sendFirst(); sendFirst(); });
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("samesamesamesame"));
});

it("allows a later beforeinput-only operation containing the same text", async () => {
  render(<Harness />);
  paste(transfer({ "text/plain": "same" }));
  await new Promise((resolve) => setTimeout(resolve, 5));
  beforePaste("same");
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("samesame"));
});

it.each(["paste", "beforeinput"])("replaces a selected directive chip through %s using Lexical's node selection", async (source) => {
  render(<Harness />);
  act(() => runtime.thread.composer.setText("@te"));
  await selectText(3, 3);
  await screen.findByText("@tester");
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", code: "Enter", keyCode: 13 });
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("@tester "));
  const editor = getNearestEditorFromDOMNode(screen.getByRole("textbox"))!;
  act(() => editor.update(() => {
    const chip = $getRoot().getChildren().flatMap((node) => $isElementNode(node) ? node.getChildren() : []).find($isDirectiveNode)!;
    const selection = $createNodeSelection();
    selection.add(chip.getKey());
    $setSelection(selection);
  }));
  if (source === "paste") paste(transfer({ "text/plain": "replacement" }));
  else beforePaste("replacement");
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("replacement"));
  expect(document.querySelector(".aui-directive-chip")).toBeNull();
});

it("uploads all pasted files once across paired events and preserves selected text", async () => {
  render(<Harness />);
  act(() => runtime.thread.composer.setText("keep this"));
  await selectText(0, 9);
  const files = [new File(["image"], "photo.png", { type: "image/png" }), new File(["file"], "notes.txt", { type: "text/plain" })];
  const data = transfer({ "text/plain": "file label" }, files);
  act(() => { paste(data); beforePaste(null, data); });
  await waitFor(() => expect(runtime.thread.composer.getState().attachments).toHaveLength(2));
  expect(mocks.uploadFile).toHaveBeenCalledTimes(2);
  expect(runtime.thread.composer.getState().text).toBe("keep this");
  expect(screen.getByText("photo.png")).toBeInTheDocument();
  expect(screen.getByText("notes.txt")).toBeInTheDocument();
});

it("keeps pasted upload failures visible and removable", async () => {
  mocks.uploadFile.mockRejectedValueOnce(new Error("Network unavailable"));
  render(<Harness />);
  paste(transfer({}, [new File(["x"], "failed.txt")]));
  expect(await screen.findByRole("alert")).toHaveTextContent("Network unavailable");
  expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
  fireEvent.click(screen.getByRole("button", { name: "Remove attachment" }));
  await waitFor(() => expect(runtime.thread.composer.getState().attachments).toHaveLength(0));
});

it("surfaces failures before an attachment can be registered", async () => {
  render(<Harness />);
  const uuid = vi.spyOn(crypto, "randomUUID").mockImplementationOnce(() => { throw new Error("Cannot stage file"); });
  paste(transfer({}, [new File(["x"], "failed.txt")]));
  expect(await screen.findByRole("alert")).toHaveTextContent("Cannot stage file");
  uuid.mockRestore();
});

it("leaves composition input to Lexical without applying paste normalization", async () => {
  render(<Harness />);
  act(() => runtime.thread.composer.setText("語"));
  await selectText(1, 1);
  fireEvent.compositionStart(screen.getByRole("textbox"), { data: "" });
  const event = new InputEvent("beforeinput", { inputType: "insertCompositionText", data: "語\u200d", isComposing: true, bubbles: true, cancelable: true });
  fireEvent(screen.getByRole("textbox"), event);
  expect(event.defaultPrevented).toBe(false);
  expect(mocks.uploadFile).not.toHaveBeenCalled();
  fireEvent.compositionEnd(screen.getByRole("textbox"), { data: "語" });
});


it("accepts beforeinput HTML and items-only file payloads", async () => {
  render(<Harness />);
  beforePaste(null, transfer({ "text/html": "<p>Mobile</p><p>HTML</p>" }));
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("Mobile\nHTML"));
  const file = new File(["image"], "items-only.png", { type: "image/png" });
  const data = { ...transfer(), items: [{ kind: "file", getAsFile: () => file }] };
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", { value: data });
  fireEvent(screen.getByRole("textbox"), event);
  await waitFor(() => expect(mocks.uploadFile).toHaveBeenCalledWith(file));
  expect(runtime.thread.composer.getState().text).toBe("Mobile\nHTML");
});

it("does not suppress another paste after an intentional selection change", async () => {
  render(<Harness />);
  act(() => {
    paste(transfer({ "text/plain": "same" }));
    const editor = getNearestEditorFromDOMNode(screen.getByRole("textbox"))!;
    editor.update(() => $getRoot().selectStart());
    beforePaste("same");
  });
  await waitFor(() => expect(runtime.thread.composer.getState().text).toBe("samesame"));
});
