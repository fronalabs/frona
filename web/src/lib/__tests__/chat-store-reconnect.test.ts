import { beforeEach, expect, it, vi } from "vitest";
import { ChatStore } from "../chat-store";
import type { MessageResponse } from "../types";

const api = vi.hoisted(() => ({ get: vi.fn() }));
vi.mock("../api-client", () => ({ api }));

const executing: MessageResponse = {
  id: "agent-1", chat_id: "chat-1", role: "agent", content: "", status: "executing",
  created_at: "2026-10-20T00:00:00Z",
};
const completed: MessageResponse = { ...executing, content: "Finished", status: "completed" };

function history(messages: MessageResponse[]) {
  api.get.mockImplementation(async (path: string) => path.endsWith("/usage") ? {} : { messages, has_more: false });
}

beforeEach(() => vi.clearAllMocks());

it.each(["completed", "failed", "cancelled", "paused"] as const)("reconciles a stored %s message and clears stale streaming/retry state", async (status) => {
  const store = new ChatStore();
  store.messages = [executing];
  store.handleEvent({ type: "token", content: "partial" });
  store.handleEvent({ type: "reasoning", content: "thinking" });
  store.handleEvent({ type: "retry", reason: "retry", retryAfterSecs: 60 });
  history([{ ...completed, status }]);
  await store.loadMessages("chat-1");
  expect(store.isRunning).toBe(false);
  expect(store.streamingText).toBe("");
  expect(store.streamingReasoning).toBe("");
  expect(store.retryInfo).toBeNull();
  expect(store.getDisplayMessages()).toEqual([{ ...completed, status }]);
});

it("restores a reopened executing chat and keeps subsequent streaming in the same message", async () => {
  const store = new ChatStore();
  history([executing]);
  await store.loadMessages("chat-1");
  expect(store.isRunning).toBe(true);
  store.handleEvent({ type: "token", content: "current" });
  expect(store.getDisplayMessages()).toMatchObject([{ id: "agent-1", content: "current", status: "executing" }]);
});

it("preserves an optimistic pending send while old completed history refreshes", async () => {
  const store = new ChatStore();
  store.messages = [executing];
  store.addUserMessage("Please continue");
  history([completed]);
  await store.loadMessages("chat-1");
  expect(store.isRunning).toBe(true);
  expect(store.messages).toMatchObject([completed, { role: "user", content: "Please continue", id: expect.stringMatching(/^__user_/) }]);
});

it("does not treat a known old terminal message as completion of a newer active run", async () => {
  const store = new ChatStore();
  store.messages = [completed];
  store.handleEvent({ type: "inference_start" });
  store.handleEvent({ type: "token", content: "new response" });
  history([completed]);
  await store.loadMessages("chat-1");
  expect(store.isRunning).toBe(true);
  expect(store.streamingText).toBe("new response");
});

it("does not let cancelled history arriving late overwrite newer live activity", async () => {
  let resolve!: (value: unknown) => void;
  api.get.mockImplementation((path: string) => path.endsWith("/usage") ? Promise.resolve({}) : new Promise((done) => { resolve = done; }));
  const store = new ChatStore();
  store.messages = [executing];
  const loading = store.loadMessages("chat-1");
  store.handleEvent({ type: "inference_done", message: completed });
  store.handleEvent({ type: "inference_start" });
  store.handleEvent({ type: "token", content: "new response" });
  resolve({ messages: [{ ...executing, status: "cancelled" }], has_more: false });
  await loading;
  expect(store.messages).toEqual([completed]);
  expect(store.streamingText).toBe("new response");
  expect(store.isRunning).toBe(true);
});

it("ignores a superseded history request from an earlier reconnect", async () => {
  const responses: ((value: unknown) => void)[] = [];
  api.get.mockImplementation((path: string) => path.endsWith("/usage") ? Promise.resolve({}) : new Promise((resolve) => responses.push(resolve)));
  const store = new ChatStore();
  const first = store.loadMessages("chat-1");
  const second = store.loadMessages("chat-1");
  responses[1]({ messages: [completed], has_more: false });
  await second;
  responses[0]({ messages: [executing], has_more: true });
  await first;
  expect(store.messages).toEqual([completed]);
  expect(store.isRunning).toBe(false);
  expect(store.hasMore).toBe(false);
});

it("keeps current activity when history fails or provides no completion evidence", async () => {
  const store = new ChatStore();
  store.handleEvent({ type: "token", content: "still running" });
  api.get.mockRejectedValue(new Error("offline"));
  await store.loadMessages("chat-1");
  history([]);
  await store.loadMessages("chat-1");
  expect(store.isRunning).toBe(true);
  expect(store.streamingText).toBe("still running");
});
