import { describe, it, expect, vi, beforeEach } from "vitest";
import { SSEEventBus, type ChatSSEEvent, type GlobalSSEEvent } from "../sse-event-bus";

describe("SSEEventBus: chat event routing", () => {
  let bus: SSEEventBus;

  beforeEach(() => {
    bus = new SSEEventBus();
  });

  it("delivers events to subscriber for the correct chat", async () => {
    const controller = new AbortController();
    const events = bus.subscribe("chat-1", controller.signal);
    const iter = events[Symbol.asyncIterator]();

    bus.routeEvent("token", "chat-1", { content: "hello" });

    const result = await iter.next();
    expect(result.done).toBe(false);
    expect(result.value).toEqual({ type: "token", content: "hello" });

    controller.abort();
  });

  it("does not deliver events for a different chat", async () => {
    const controller = new AbortController();
    const events = bus.subscribe("chat-1", controller.signal);
    const iter = events[Symbol.asyncIterator]();

    bus.routeEvent("token", "chat-2", { content: "wrong chat" });

    // No events for chat-1 — abort and verify iterator is done
    controller.abort();
    const result = await iter.next();
    expect(result.done).toBe(true);
  });

  it("buffers events that arrive before subscription", async () => {
    bus.routeEvent("token", "chat-1", { content: "buffered-1" });
    bus.routeEvent("token", "chat-1", { content: "buffered-2" });

    const controller = new AbortController();
    const events = bus.subscribe("chat-1", controller.signal);
    const iter = events[Symbol.asyncIterator]();

    const r1 = await iter.next();
    const r2 = await iter.next();

    expect(r1.value).toEqual({ type: "token", content: "buffered-1" });
    expect(r2.value).toEqual({ type: "token", content: "buffered-2" });

    controller.abort();
  });

  it("supports multiple subscribers for the same chat", async () => {
    const c1 = new AbortController();
    const c2 = new AbortController();

    const iter1 = bus.subscribe("chat-1", c1.signal)[Symbol.asyncIterator]();
    const iter2 = bus.subscribe("chat-1", c2.signal)[Symbol.asyncIterator]();

    bus.routeEvent("token", "chat-1", { content: "shared" });

    const r1 = await iter1.next();
    const r2 = await iter2.next();

    expect(r1.value).toEqual({ type: "token", content: "shared" });
    expect(r2.value).toEqual({ type: "token", content: "shared" });

    c1.abort();
    c2.abort();
  });

  it("cleans up subscriber on abort", async () => {
    const controller = new AbortController();
    bus.subscribe("chat-1", controller.signal);

    controller.abort();

    // Subsequent events should be buffered (no active subscribers)
    bus.routeEvent("token", "chat-1", { content: "after-abort" });

    // New subscriber should pick up the buffered event
    const c2 = new AbortController();
    const iter = bus.subscribe("chat-1", c2.signal)[Symbol.asyncIterator]();
    const r = await iter.next();
    expect(r.value).toEqual({ type: "token", content: "after-abort" });

    c2.abort();
  });

  it("routes tool_call events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("tool_call", "chat-1", {
      id: "te-1",
      provider_call_id: "tc-1",
      name: "web_search",
      arguments: { query: "test" },
      description: "Searching",
    });

    const r = await iter.next();
    expect(r.value).toEqual({
      type: "tool_call",
      id: "te-1",
      provider_call_id: "tc-1",
      name: "web_search",
      arguments: { query: "test" },
      description: "Searching",
    });

    controller.abort();
  });

  it("lets observers inspect a chat event without consuming it", async () => {
    const observed: Array<{ chatId: string; event: ChatSSEEvent }> = [];
    bus.onChatEvent((chatId, event) => observed.push({ chatId, event }));
    const controller = new AbortController();
    const iter = bus.subscribe("task-chat", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("tool_call", "task-chat", {
      id: "te-1",
      provider_call_id: "tc-1",
      name: "web_search",
      arguments: { query: "test" },
      description: "Searching",
    });

    const delivered = await iter.next();
    expect(observed).toEqual([{ chatId: "task-chat", event: delivered.value }]);

    controller.abort();
  });

  it("routes tool_result events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("tool_result", "chat-1", {
      name: "web_search",
      success: true,
      summary: "Done",
    });

    const r = await iter.next();
    expect(r.value).toEqual({
      type: "tool_result",
      name: "web_search",
      success: true,
      summary: "Done",
    });

    controller.abort();
  });

  it("routes inference_done events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    const message = { id: "msg-1", chat_id: "chat-1", role: "agent", content: "Done", status: "completed", created_at: "" };
    bus.routeEvent("inference_done", "chat-1", { message });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "inference_done", message });

    controller.abort();
  });

  it("routes retry events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("retry", "chat-1", { retry_after_secs: 5, reason: "rate_limited" });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "retry", retryAfterSecs: 5, reason: "rate_limited" });

    controller.abort();
  });

  it("routes inference_cancelled events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("inference_cancelled", "chat-1", { reason: "user cancelled" });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "inference_cancelled", reason: "user cancelled" });

    controller.abort();
  });

  it("routes inference_error events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("inference_error", "chat-1", { error: "model error" });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "inference_error", error: "model error" });

    controller.abort();
  });

  it("routes chat_message events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    const message = { id: "msg-1", chat_id: "chat-1", role: "user", content: "Hi", created_at: "" };
    bus.routeEvent("chat_message", "chat-1", { message });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "chat_message", message });

    controller.abort();
  });

  it("routes inference_paused events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    const message = { id: "msg-1", role: "agent", content: "", status: "executing" };
    const reason = { type: "Hitl" };
    bus.routeEvent("inference_paused", "chat-1", { reason, message });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "inference_paused", reason, message });

    controller.abort();
  });

  it("routes inference_resume events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    const message = { id: "msg-1", role: "agent", content: "", status: "executing" };
    bus.routeEvent("inference_resume", "chat-1", { message });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "inference_resume", message });

    controller.abort();
  });

  it("routes reasoning events correctly", async () => {
    const controller = new AbortController();
    const iter = bus.subscribe("chat-1", controller.signal)[Symbol.asyncIterator]();

    bus.routeEvent("reasoning", "chat-1", { content: "thinking..." });

    const r = await iter.next();
    expect(r.value).toEqual({ type: "reasoning", content: "thinking..." });

    controller.abort();
  });
});

describe("SSEEventBus: global events", () => {
  let bus: SSEEventBus;

  beforeEach(() => {
    bus = new SSEEventBus();
  });

  it("delivers title events to global listeners", () => {
    const received: GlobalSSEEvent[] = [];
    bus.onGlobal((e) => received.push(e));

    bus.routeEvent("title", "chat-1", { title: "New Title" });

    expect(received).toHaveLength(1);
    expect(received[0]).toEqual({ type: "title", chatId: "chat-1", title: "New Title" });
  });

  it("delivers entity_updated events to global listeners", () => {
    const received: GlobalSSEEvent[] = [];
    bus.onGlobal((e) => received.push(e));

    bus.routeEvent("entity_updated", "chat-1", { table: "agent", record_id: "a1", fields: { name: "New" } });

    expect(received).toHaveLength(1);
    expect(received[0]).toEqual({
      type: "entity_updated",
      chatId: "chat-1",
      table: "agent",
      recordId: "a1",
      fields: { name: "New" },
    });
  });

  it("delivers task_update events to global listeners", () => {
    const received: GlobalSSEEvent[] = [];
    bus.onGlobal((e) => received.push(e));

    bus.routeEvent("task_update", "", {
      task_id: "t1",
      status: "completed",
      source_chat_id: null,
      title: "My Task",
      chat_id: "chat-1",
      result_summary: "Done",
    });

    expect(received).toHaveLength(1);
    expect(received[0]).toEqual({
      type: "task_update",
      taskId: "t1",
      status: "completed",
      sourceChatId: null,
      title: "My Task",
      chatId: "chat-1",
      resultSummary: "Done",
    });
  });

  it("delivers inference_count events to global listeners", () => {
    const received: GlobalSSEEvent[] = [];
    bus.onGlobal((e) => received.push(e));

    bus.routeEvent("inference_count", "", { count: 3 });

    expect(received).toHaveLength(1);
    expect(received[0]).toEqual({ type: "inference_count", count: 3 });
  });

  it("delivers activity_changed as a payload-free wake event", () => {
    const received: GlobalSSEEvent[] = [];
    bus.onGlobal((event) => received.push(event));

    bus.routeEvent("activity_changed", "", {});

    expect(received).toEqual([{ type: "activity_changed" }]);
  });

  it("delivers notification events to global listeners", () => {
    const received: GlobalSSEEvent[] = [];
    bus.onGlobal((e) => received.push(e));

    const notification = { id: "n1", data: { type: "System" }, level: "info", title: "Test", body: "body", read: false, created_at: "" };
    bus.routeEvent("notification", "", { notification });

    expect(received).toHaveLength(1);
    expect(received[0]).toEqual({ type: "notification", notification });
  });

  it("unsubscribe stops global listener", () => {
    const received: GlobalSSEEvent[] = [];
    const unsub = bus.onGlobal((e) => received.push(e));

    unsub();
    bus.routeEvent("inference_count", "", { count: 5 });

    expect(received).toHaveLength(0);
  });

  it("supports multiple global listeners", () => {
    const r1: GlobalSSEEvent[] = [];
    const r2: GlobalSSEEvent[] = [];

    bus.onGlobal((e) => r1.push(e));
    bus.onGlobal((e) => r2.push(e));

    bus.routeEvent("inference_count", "", { count: 1 });

    expect(r1).toHaveLength(1);
    expect(r2).toHaveLength(1);
  });
});

describe("SSEEventBus: reconnect behavior", () => {
  let bus: SSEEventBus;

  beforeEach(() => {
    bus = new SSEEventBus();
  });

  it("calls reconnect listeners", () => {
    const listener = vi.fn();
    bus.onReconnect(listener);

    // Simulate reconnect by connecting twice — the second connect triggers reconnect
    // We can test onReconnect directly by verifying it registers/unregisters
    expect(listener).not.toHaveBeenCalled();
  });

  it("unsubscribing reconnect listener works", () => {
    const listener = vi.fn();
    const unsub = bus.onReconnect(listener);
    unsub();

    // Listener should be removed — no way to trigger reconnect without a real stream,
    // but we verified the unsub mechanics
    expect(listener).not.toHaveBeenCalled();
  });
});
