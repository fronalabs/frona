import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { StrictMode, useEffect } from "react";
import { AssistantRuntimeProvider, ThreadPrimitive, useExternalStoreRuntime, type AssistantRuntime, type CompleteAttachment } from "@assistant-ui/react";
import { beforeEach, expect, it, vi } from "vitest";
import { registerBackendAttachment, useChatRuntime } from "@/lib/use-chat-runtime";
import type { Attachment, MessageResponse } from "@/lib/types";
import { FronaUserMessage } from "../frona-user-message";

const mocks = vi.hoisted(() => ({ uploadFile: vi.fn(), sendMessage: vi.fn(), presignFile: vi.fn(), history: [] as MessageResponse[] }));
vi.mock("@/lib/api-client", () => ({
  api: { get: vi.fn(async () => ({ messages: mocks.history, has_more: false })) },
  uploadFile: mocks.uploadFile, sendMessage: mocks.sendMessage, presignFile: mocks.presignFile, cancelGeneration: vi.fn(),
}));
vi.mock("@/lib/format-time", () => ({ computeTimeMarkers: () => new Map(), useTimezone: () => "UTC", formatFullTimestamp: () => "Now", formatTime: () => "Now" }));
vi.mock("@/lib/navigation-context", () => ({ useNavigation: () => ({ agents: [] }) }));
vi.mock("../markdown-text", () => ({ MarkdownText: () => null }));

let runtime: AssistantRuntime;
function Messages({ runtime }: { runtime: AssistantRuntime }) {
  return <AssistantRuntimeProvider runtime={runtime}><ThreadPrimitive.Root><ThreadPrimitive.Messages components={{ UserMessage: FronaUserMessage, AssistantMessage: () => null }} /></ThreadPrimitive.Root></AssistantRuntimeProvider>;
}
function Chat() {
  const chat = useChatRuntime({ chatId: "chat-preview", agentId: "agent" });
  useEffect(() => { runtime = chat.runtime; }, [chat.runtime]);
  return <Messages runtime={chat.runtime} />;
}
function Preview({ attachment }: { attachment?: CompleteAttachment }) {
  const runtime = useExternalStoreRuntime({ messages: [{ id: "preview", role: "user" as const, content: [], attachments: attachment ? [attachment] : [] }], convertMessage: (message) => message, onNew: async () => {} });
  return <Messages runtime={runtime} />;
}
function attachment(overrides: Partial<CompleteAttachment> = {}): CompleteAttachment {
  return { id: crypto.randomUUID(), type: "file", name: "photo.png", contentType: "image/png", status: { type: "complete" }, content: [{ type: "text", text: "[file: photo.png]" }], ...overrides };
}
const backend: Attachment = { path: "uploads/render-preview", filename: "photo.png", content_type: "image/png", size_bytes: 1, owner: "user" };

beforeEach(() => {
  vi.clearAllMocks();
  mocks.history = [];
  mocks.presignFile.mockResolvedValue("");
  let nextBlob = 0;
  URL.createObjectURL = vi.fn(() => `blob:preview-${++nextBlob}`);
  URL.revokeObjectURL = vi.fn();
});

it("displays the upload's signed image immediately after send and after history reload", async () => {
  const uploaded = { ...backend, url: "https://files.example/photo.png?presign=upload" };
  mocks.uploadFile.mockResolvedValueOnce(uploaded);
  const view = render(<Chat />);
  await act(() => runtime.thread.composer.addAttachment(new File(["x"], "photo.png", { type: "image/png" })));
  act(() => runtime.thread.composer.send());
  expect(await screen.findByRole("img")).toHaveAttribute("src", uploaded.url);
  expect(URL.createObjectURL).not.toHaveBeenCalled();
  expect(mocks.presignFile).not.toHaveBeenCalled();
  view.unmount();
  mocks.history = [{ id: "saved", chat_id: "chat-preview", role: "user", content: "", created_at: "2026-10-20T00:00:00Z", attachments: [uploaded] }];
  render(<Chat />);
  expect(await screen.findByRole("img")).toHaveAttribute("src", uploaded.url);
  expect(document.querySelector('img[src=""]')).toBeNull();
});

it("renders a URL-less live upload locally and revokes its URL on unmount", async () => {
  mocks.uploadFile.mockResolvedValueOnce(backend);
  const view = render(<Chat />);
  const file = new File(["x"], "photo.png", { type: "image/png" });
  await act(() => runtime.thread.composer.addAttachment(file));
  await act(async () => runtime.thread.composer.send());
  const previewUrl = screen.getByRole("img").getAttribute("src");
  const created = vi.mocked(URL.createObjectURL).mock.results.map((result) => result.value);
  expect(created).toContain(previewUrl);
  expect(vi.mocked(URL.createObjectURL).mock.calls.every(([blob]) => blob === file)).toBe(true);
  expect(URL.revokeObjectURL).not.toHaveBeenCalledWith(previewUrl);
  expect(mocks.sendMessage).toHaveBeenCalledWith("chat-preview", { content: "", attachments: [backend] });
  view.unmount();
  // Sending can replace the submission preview with the persisted message preview.
  expect(vi.mocked(URL.revokeObjectURL).mock.calls.map(([url]) => url).sort()).toEqual(created.sort());
});

it.each([undefined, "", "   "])("renders a named placeholder without a URL or local file (%s)", (url) => {
  const data = attachment({ content: url === undefined ? [] : [{ type: "image", image: url }] });
  render(<Preview attachment={data} />);
  expect(screen.queryByRole("img")).toBeNull();
  expect(screen.getByText("photo.png")).toBeInTheDocument();
  expect(URL.createObjectURL).not.toHaveBeenCalled();
});

it.each(["text/plain", ""])("does not create image blobs for a local file with MIME %s", (type) => {
  render(<Preview attachment={attachment({ file: new File(["x"], "notes.txt", { type }) })} />);
  expect(screen.queryByRole("img")).toBeNull();
  expect(URL.createObjectURL).not.toHaveBeenCalled();
});

it("revokes a fallback exactly once when the attachment is removed", () => {
  const view = render(<Preview attachment={attachment({ file: new File(["x"], "photo.png", { type: "image/png" }) })} />);
  expect(screen.getByRole("img")).toHaveAttribute("src", "blob:preview-1");
  view.rerender(<Preview />);
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:preview-1");
  view.unmount();
  expect(URL.revokeObjectURL).toHaveBeenCalledTimes(1);
});

it("replaces a local preview with a presigned URL and revokes the blob once on unmount", async () => {
  let resolve!: (url: string) => void;
  mocks.presignFile.mockImplementationOnce(() => new Promise<string>((done) => { resolve = done; }));
  const data = attachment({ file: new File(["x"], "photo.png", { type: "image/png" }) });
  registerBackendAttachment(data.id, backend);
  const view = render(<Preview attachment={data} />);
  expect(screen.getByRole("img")).toHaveAttribute("src", "blob:preview-1");
  await act(async () => resolve("https://files.example/fresh.png"));
  expect(screen.getByRole("img")).toHaveAttribute("src", "https://files.example/fresh.png");
  expect(URL.revokeObjectURL).not.toHaveBeenCalled();
  view.unmount();
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:preview-1");
});

it("retains MIME-based image rendering and refreshes a failed signed URL once", async () => {
  const data = attachment();
  registerBackendAttachment(data.id, { ...backend, url: "https://files.example/expired.png" });
  mocks.presignFile.mockResolvedValueOnce("https://files.example/fresh.png");
  render(<Preview attachment={data} />);
  expect(screen.getByRole("img")).toHaveAttribute("src", "https://files.example/expired.png");
  fireEvent.error(screen.getByRole("img"));
  await waitFor(() => expect(screen.getByRole("img")).toHaveAttribute("src", "https://files.example/fresh.png"));
  fireEvent.error(screen.getByRole("img"));
  expect(mocks.presignFile).toHaveBeenCalledOnce();
  expect(URL.createObjectURL).not.toHaveBeenCalled();
});

it("does not allocate previews for a failed upload or its removal", async () => {
  mocks.uploadFile.mockRejectedValueOnce(new Error("Upload failed"));
  const view = render(<Chat />);
  await act(() => runtime.thread.composer.addAttachment(new File(["x"], "photo.png", { type: "image/png" })));
  expect(runtime.thread.composer.getState().attachments[0].status.type).toBe("incomplete");
  await act(() => runtime.thread.composer.clearAttachments());
  view.unmount();
  expect(URL.createObjectURL).not.toHaveBeenCalled();
  expect(URL.revokeObjectURL).not.toHaveBeenCalled();
});

it("revokes every fallback once across Strict Mode effect cleanup", () => {
  const file = new File(["x"], "photo.png", { type: "image/png" });
  const view = render(<StrictMode><Preview attachment={attachment({ file })} /></StrictMode>);
  expect(screen.getByRole("img").getAttribute("src")).toMatch(/^blob:/);
  view.unmount();
  const created = vi.mocked(URL.createObjectURL).mock.results.map((result) => result.value);
  expect(created.length).toBeGreaterThan(0);
  expect(vi.mocked(URL.revokeObjectURL).mock.calls.map(([url]) => url).sort()).toEqual(created.sort());
});

it("ignores a presign result after removal and does not revoke a blob twice", async () => {
  let resolve!: (url: string) => void;
  mocks.presignFile.mockImplementationOnce(() => new Promise<string>((done) => { resolve = done; }));
  const data = attachment({ file: new File(["x"], "photo.png", { type: "image/png" }) });
  registerBackendAttachment(data.id, backend);
  const view = render(<Preview attachment={data} />);
  view.rerender(<Preview />);
  await act(async () => resolve("https://files.example/late.png"));
  expect(screen.queryByRole("img")).toBeNull();
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:preview-1");
  view.unmount();
  expect(URL.revokeObjectURL).toHaveBeenCalledTimes(1);
});

it("refreshes a saved ordinary file URL before opening the file chip", async () => {
  const document = { ...backend, path: "uploads/report.pdf", filename: "report.pdf", content_type: "application/pdf", url: "https://files.example/expired.pdf" };
  mocks.history = [{ id: "saved-file", chat_id: "chat-preview", role: "user", content: "", created_at: "2026-10-20T00:00:00Z", attachments: [document] }];
  mocks.presignFile.mockResolvedValueOnce("https://files.example/fresh.pdf");
  const open = vi.spyOn(window, "open").mockImplementation(() => null);
  render(<Chat />);
  await screen.findByText("report.pdf");
  await waitFor(() => expect(mocks.presignFile).toHaveBeenCalledWith("user", "uploads/report.pdf"));
  fireEvent.click(screen.getByText("report.pdf"));
  expect(open).toHaveBeenCalledWith("https://files.example/fresh.pdf", "_blank");
  expect(screen.queryByRole("img")).toBeNull();
  expect(URL.createObjectURL).not.toHaveBeenCalled();
  open.mockRestore();
});

it("returns to a placeholder when a local file is no longer available", () => {
  const data = attachment({ file: new File(["x"], "photo.png", { type: "image/png" }) });
  const view = render(<Preview attachment={data} />);
  expect(screen.getByRole("img")).toHaveAttribute("src", "blob:preview-1");
  view.rerender(<Preview attachment={{ ...data, file: undefined }} />);
  expect(screen.queryByRole("img")).toBeNull();
  expect(screen.getByText("photo.png")).toBeInTheDocument();
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:preview-1");
  view.unmount();
  expect(URL.revokeObjectURL).toHaveBeenCalledTimes(1);
});
