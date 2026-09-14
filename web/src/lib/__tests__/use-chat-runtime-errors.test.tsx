import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { useChatRuntime } from "../use-chat-runtime";

const mocks = vi.hoisted(() => ({ sendMessage: vi.fn(), post: vi.fn() }));
vi.mock("../api-client", () => ({
  api: {
    get: vi.fn(async () => ({ messages: [], has_more: false })),
    post: mocks.post,
  },
  sendMessage: mocks.sendMessage,
  cancelGeneration: vi.fn(),
  uploadFile: vi.fn(),
}));
vi.mock("../format-time", () => ({
  computeTimeMarkers: () => new Map(),
  useTimezone: () => "UTC",
}));

beforeEach(() => vi.clearAllMocks());

it("shows a message request error through the chat runtime", async () => {
  mocks.sendMessage.mockRejectedValueOnce(new Error("Failed to process message"));
  const { result } = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await waitFor(() => expect(result.current.loaded).toBe(true));
  act(() => result.current.sendMessage("Hello"));
  await waitFor(() => {
    const thread = result.current.runtime.thread.getState();
    expect(thread.isRunning).toBe(false);
    expect(thread.messages.at(-1)?.status).toMatchObject({
      type: "incomplete", reason: "error", error: { message: "Failed to process message", timestamp: expect.any(String), details: { subsystem: "message_processing" } },
    });
  });
});

it("shows chat creation failures instead of dropping the send", async () => {
  mocks.post.mockRejectedValueOnce(new Error("Unable to create chat"));
  const { result } = renderHook(() => useChatRuntime({ agentId: "agent-1", onChatCreated: vi.fn() }));
  act(() => result.current.sendMessage("Hello"));
  await waitFor(() => {
    expect(result.current.runtime.thread.getState().messages.at(-1)?.status).toMatchObject({
      type: "incomplete", reason: "error", error: { message: "Unable to create chat", timestamp: expect.any(String), details: { subsystem: "message_processing" } },
    });
  });
  expect(mocks.sendMessage).not.toHaveBeenCalled();
});
