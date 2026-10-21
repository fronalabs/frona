import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useChatRuntime } from "../use-chat-runtime";
import { sseBus } from "../sse-event-bus";
import type { MessageResponse } from "../types";

const mocks = vi.hoisted(() => ({ get: vi.fn(), sendMessage: vi.fn() }));
vi.mock("../api-client", () => ({
  api: { get: mocks.get, post: vi.fn() },
  sendMessage: mocks.sendMessage, cancelGeneration: vi.fn(), uploadFile: vi.fn(),
  ensureAccessToken: vi.fn(async () => ({ ok: false })), API_URL: "",
}));
vi.mock("../format-time", () => ({ computeTimeMarkers: () => new Map(), useTimezone: () => "UTC" }));

let connection: AbortController;
let streams: ReadableStreamDefaultController<Uint8Array>[];
let messages: MessageResponse[];
const executing: MessageResponse = {
  id: "agent-1", chat_id: "chat-1", role: "agent", content: "", status: "executing",
  created_at: "2026-10-20T00:00:00Z",
};

beforeEach(async () => {
  vi.clearAllMocks();
  vi.useFakeTimers();
  messages = [executing];
  streams = [];
  connection = new AbortController();
  mocks.get.mockImplementation(async (path: string) => path.endsWith("/usage") ? {
    totals: { input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, cost_usd: 0, calls: 0, incomplete_usage_calls: 0 },
    last_chat_input_tokens: null, total_tool_calls: 0,
  } : { messages, has_more: false });
  vi.stubGlobal("fetch", vi.fn(async (_url: string, options: RequestInit) => ({
    ok: true,
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        streams.push(controller);
        options.signal?.addEventListener("abort", () => controller.error(new DOMException("Aborted", "AbortError")));
      },
    }),
  })));
  sseBus.connect(connection.signal);
  await vi.advanceTimersByTimeAsync(0);
});

afterEach(async () => {
  connection.abort();
  await vi.advanceTimersByTimeAsync(0);
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function reconnect() {
  await act(async () => {
    streams.at(-1)!.error(new Error("network disconnected"));
    await vi.advanceTimersByTimeAsync(1000);
  });
}

async function emit(type: string, data: Record<string, unknown>) {
  await act(async () => {
    streams.at(-1)!.enqueue(new TextEncoder().encode(
      `event: ${type}\ndata: ${JSON.stringify({ chat_id: "chat-1", ...data })}\n\n`,
    ));
    await vi.advanceTimersByTimeAsync(20);
  });
}

it("keeps the mounted runtime subscribed, reconciles disconnected completion, and stops loading after unmount", async () => {
  const hook = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  expect(hook.result.current.runtime.thread.getState().isRunning).toBe(true);
  await emit("token", { content: "partial" });
  expect(hook.result.current.runtime.thread.getState().messages.at(-1)?.content).toContainEqual({ type: "text", text: "partial" });

  messages = [{ ...executing, status: "completed", content: "Completed while offline" }];
  await reconnect();
  expect(hook.result.current.runtime.thread.getState().isRunning).toBe(false);
  expect(hook.result.current.runtime.thread.getState().messages).toHaveLength(1);
  expect(hook.result.current.runtime.thread.getState().messages[0].content).toContainEqual({ type: "text", text: "Completed while offline" });

  await emit("inference_start", {});
  await emit("token", { content: "new activity" });
  expect(hook.result.current.runtime.thread.getState().isRunning).toBe(true);
  expect(hook.result.current.runtime.thread.getState().messages.at(-1)?.content).toContainEqual({ type: "text", text: "new activity" });
  await reconnect();
  await emit("token", { content: " continues" });
  expect(hook.result.current.runtime.thread.getState().messages.at(-1)?.content).toContainEqual({ type: "text", text: "new activity continues" });

  hook.unmount();
  const requestCount = mocks.get.mock.calls.length;
  await reconnect();
  await emit("inference_cancelled", { reason: "stopped after unmount" });
  expect(mocks.get).toHaveBeenCalledTimes(requestCount);
});

it("retains an optimistic pending send when reconnect returns older completed history", async () => {
  messages = [{ ...executing, status: "completed", content: "Earlier answer" }];
  mocks.sendMessage.mockReturnValue(new Promise(() => {}));
  const hook = renderHook(() => useChatRuntime({ chatId: "chat-1", agentId: "agent-1" }));
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  act(() => hook.result.current.sendMessage("My pending question"));
  await reconnect();
  const thread = hook.result.current.runtime.thread.getState();
  expect(thread.isRunning).toBe(true);
  expect(thread.messages.some((message) => message.role === "user" && message.content.some((part) => part.type === "text" && part.text === "My pending question"))).toBe(true);
  hook.unmount();
});
