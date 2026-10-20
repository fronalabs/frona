import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { convertMessage, fronaAttachmentAdapter, registerBackendAttachment, useChatRuntime } from "../use-chat-runtime";
import type { Attachment, MessageResponse } from "../types";

const mocks = vi.hoisted(() => ({ uploadFile: vi.fn(), sendMessage: vi.fn() }));
vi.mock("../api-client", () => ({ api: { get: vi.fn(async () => ({ messages: [], has_more: false })) }, uploadFile: mocks.uploadFile, sendMessage: mocks.sendMessage, cancelGeneration: vi.fn() }));
vi.mock("../format-time", () => ({ computeTimeMarkers: () => new Map(), useTimezone: () => "UTC" }));

beforeEach(() => { vi.clearAllMocks(); URL.createObjectURL = vi.fn(() => "blob:unused"); });
const backend: Attachment = { path: "uploads/preview", filename: "photo.png", content_type: "image/png", size_bytes: 1, owner: "user" };
function history(attachment: Attachment) {
  const message = convertMessage({ id: "history", chat_id: "chat-1", role: "user", content: "", created_at: "2026-10-20T00:00:00Z", attachments: [attachment] } as MessageResponse);
  return message?.role === "user" ? message.attachments?.[0] : undefined;
}

it.each([
  ["image/png", "https://files.example/photo.png?presign=token", "image"],
  ["image/png", undefined, "file"],
  ["image/png", "", "file"],
  ["image/png", "   ", "file"],
  ["text/plain", "https://files.example/note.txt", "file"],
])("agrees on completion and history for %s / %s", async (mime, url, type) => {
  const uploaded = { ...backend, content_type: mime, url };
  registerBackendAttachment("pending-preview", uploaded);
  const complete = await fronaAttachmentAdapter.send({ id: "pending-preview", name: uploaded.filename, type: "file", contentType: mime, file: new File(["x"], uploaded.filename, { type: mime }), status: { type: "requires-action", reason: "composer-send" } });
  expect(complete.type).toBe(type);
  expect(complete.content).toEqual(history(uploaded)?.content);
  expect(history(uploaded)?.type).toBe(type);
  expect(URL.createObjectURL).not.toHaveBeenCalled();
});

it("preserves a local image file through the real optimistic message conversion without serializing it", async () => {
  mocks.uploadFile.mockResolvedValueOnce(backend);
  const file = new File(["x"], "photo.png", { type: "image/png" });
  const { result, unmount } = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await waitFor(() => expect(result.current.loaded).toBe(true));
  await act(() => result.current.runtime.thread.composer.addAttachment(file));
  act(() => result.current.runtime.thread.composer.send());
  await waitFor(() => expect(mocks.sendMessage).toHaveBeenCalledOnce());
  const message = result.current.runtime.thread.getState().messages.find((message) => message.role === "user");
  expect(message?.attachments[0]).toMatchObject({ type: "file", file, content: [{ type: "text", text: "[file: photo.png]" }] });
  expect(mocks.sendMessage).toHaveBeenCalledWith("chat-1", { content: "", attachments: [backend] });
  expect(JSON.stringify(mocks.sendMessage.mock.calls)).not.toContain("blob:");
  expect(URL.createObjectURL).not.toHaveBeenCalled();
  unmount();
});

it.each([undefined, "https://files.example/without-local-file.png"])("completes an attachment without a local File (%s)", async (url) => {
  registerBackendAttachment("no-local-file", { ...backend, url });
  const complete = await fronaAttachmentAdapter.send({ id: "no-local-file", name: "photo.png", type: "file", contentType: "image/png", status: { type: "requires-action", reason: "composer-send" } } as Parameters<typeof fronaAttachmentAdapter.send>[0]);
  expect(complete.type).toBe(url ? "image" : "file");
  expect(complete.content).toEqual(url ? [{ type: "image", image: url }] : [{ type: "text", text: "[file: photo.png]" }]);
  expect(URL.createObjectURL).not.toHaveBeenCalled();
});
