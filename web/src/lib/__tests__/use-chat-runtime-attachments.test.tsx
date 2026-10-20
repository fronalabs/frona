import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { useChatRuntime } from "../use-chat-runtime";

const mocks = vi.hoisted(() => ({ uploadFile: vi.fn(), sendMessage: vi.fn() }));
vi.mock("../api-client", () => ({
  api: { get: vi.fn(async () => ({ messages: [], has_more: false })) },
  uploadFile: mocks.uploadFile,
  sendMessage: mocks.sendMessage,
  cancelGeneration: vi.fn(),
}));
vi.mock("../format-time", () => ({ computeTimeMarkers: () => new Map(), useTimezone: () => "UTC" }));

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn(() => "blob:preview") }));
});

it.each([
  ["notes.txt", "text/plain"],
  ["camera.heic", "image/heic"],
  ["mobile-file", ""],
])("accepts and sends %s through the installed composer", async (name, type) => {
  const backend = { path: `uploads/${name}`, filename: name, content_type: type, size_bytes: 1, owner: "user" };
  mocks.uploadFile.mockResolvedValueOnce(backend);
  const { result } = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await waitFor(() => expect(result.current.loaded).toBe(true));
  const composer = result.current.runtime.thread.composer;
  await act(() => composer.addAttachment(new File(["x"], name, { type })));
  expect(composer.getState().attachments).toHaveLength(1);
  act(() => composer.send());
  await waitFor(() => expect(mocks.sendMessage).toHaveBeenCalledWith("chat-1", { content: "", attachments: [backend] }));
});

it("retains the draft when staged metadata is missing and allows reattachment", async () => {
  const { result } = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await waitFor(() => expect(result.current.loaded).toBe(true));
  const composer = result.current.runtime.thread.composer;
  await act(() => composer.addAttachment({ id: "missing", name: "notes.txt", type: "file", content: [] }));
  act(() => { composer.setText("Keep my file"); composer.send(); });
  await waitFor(() => expect(result.current.runtime.thread.getState().messages.at(-1)?.status).toMatchObject({
    type: "incomplete", reason: "error", error: { message: expect.stringContaining("notes.txt") },
  }));
  await waitFor(() => expect(composer.getState().text).toBe("Keep my file"));
  expect(composer.getState().attachments).toHaveLength(1);
  expect(mocks.sendMessage).not.toHaveBeenCalled();
  await act(() => composer.clearAttachments());
  mocks.uploadFile.mockResolvedValueOnce({ path: "uploads/retry", filename: "notes.txt", content_type: "text/plain", size_bytes: 1, owner: "user" });
  await act(() => composer.addAttachment(new File(["x"], "notes.txt", { type: "text/plain" })));
  act(() => composer.send());
  await waitFor(() => expect(mocks.sendMessage).toHaveBeenCalledOnce());
});

it("retains a failed upload as a removable attachment", async () => {
  mocks.uploadFile.mockRejectedValueOnce(new Error("Network unavailable"));
  const { result } = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await waitFor(() => expect(result.current.loaded).toBe(true));
  const composer = result.current.runtime.thread.composer;
  await act(() => composer.addAttachment(new File(["x"], "notes.txt", { type: "text/plain" })));
  expect(composer.getState().attachments[0]).toMatchObject({
    name: "notes.txt", status: { type: "incomplete", reason: "error", message: expect.stringContaining("Network unavailable") },
  });
  expect(mocks.sendMessage).not.toHaveBeenCalled();
  await act(() => composer.clearAttachments());
  expect(composer.getState().attachments).toHaveLength(0);
});

it("does not send a partial message when only one attachment has metadata", async () => {
  mocks.uploadFile.mockResolvedValueOnce({ path: "uploads/present", filename: "present.txt", content_type: "text/plain", size_bytes: 1, owner: "user" });
  const { result } = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await waitFor(() => expect(result.current.loaded).toBe(true));
  const composer = result.current.runtime.thread.composer;
  await act(() => composer.addAttachment(new File(["x"], "present.txt", { type: "text/plain" })));
  await act(() => composer.addAttachment({ id: "missing-partial", name: "missing.txt", content: [] }));
  act(() => { composer.setText("Both files"); composer.send(); });
  await waitFor(() => expect(result.current.runtime.thread.getState().messages.at(-1)?.status).toMatchObject({ type: "incomplete" }));
  await waitFor(() => expect(composer.getState().attachments).toHaveLength(2));
  expect(composer.getState().text).toBe("Both files");
  expect(result.current.runtime.thread.getState().messages.some((message) => message.role === "user")).toBe(false);
  expect(mocks.sendMessage).not.toHaveBeenCalled();
});
