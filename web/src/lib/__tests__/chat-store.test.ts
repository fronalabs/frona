import { describe, it, expect, vi, beforeEach } from "vitest";
import { ChatStore, mergeConsecutiveMessages } from "../chat-store";
import type { ChatSSEEvent } from "../sse-event-bus";
import type { MessageResponse, ToolCall } from "../types";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeAgentMessage(overrides: Partial<MessageResponse> = {}): MessageResponse {
  return {
    id: "msg-1",
    chat_id: "chat-1",
    role: "agent",
    content: "Hello from agent",
    status: "completed",
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function makeUserMessage(overrides: Partial<MessageResponse> = {}): MessageResponse {
  return {
    id: "msg-u1",
    chat_id: "chat-1",
    role: "user",
    content: "Hello from user",
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function makeToolCall(overrides: Partial<ToolCall> = {}): ToolCall {
  return {
    id: "te-1",
    chat_id: "chat-1",
    message_id: "msg-1",
    turn: 1,
    provider_call_id: "tc-1",
    name: "web_search",
    arguments: { query: "test" },
    result: "Found results",
    success: true,
    duration_ms: 100,
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// ChatStore: SSE event handling
// ---------------------------------------------------------------------------

describe("ChatStore", () => {
  let store: ChatStore;

  beforeEach(() => {
    store = new ChatStore();
  });

  describe("subscriber notifications", () => {
    it("notifies subscribers on state changes", async () => {
      const listener = vi.fn();
      store.subscribe(listener);

      store.handleEvent({ type: "token", content: "hi" });
      await vi.waitFor(() => expect(listener).toHaveBeenCalledTimes(1));
    });

    it("batches mixed SSE notifications to one animation frame", () => {
      const callbacks: FrameRequestCallback[] = [];
      const animationFrame = vi
        .spyOn(globalThis, "requestAnimationFrame")
        .mockImplementation((callback) => {
          callbacks.push(callback);
          return callbacks.length;
        });
      const listener = vi.fn();
      store.subscribe(listener);

      for (let i = 0; i < 100; i += 1) {
        store.handleEvent({ type: "token", content: "x" });
        store.handleEvent({
          type: "tool_call",
          id: `tool-${i}`,
          provider_call_id: `provider-tool-${i}`,
          name: `tool_${i}`,
          arguments: {},
        });
        store.handleEvent({
          type: "tool_result",
          name: `tool_${i}`,
          success: true,
        });
      }

      expect(listener).not.toHaveBeenCalled();
      expect(callbacks).toHaveLength(1);
      callbacks[0](0);
      expect(listener).toHaveBeenCalledTimes(1);
      animationFrame.mockRestore();
    });

    it("unsubscribe stops notifications", () => {
      const listener = vi.fn();
      const unsub = store.subscribe(listener);
      unsub();

      store.handleEvent({ type: "token", content: "hi" });
      expect(listener).not.toHaveBeenCalled();
    });

    it("defers subscribers added during notification until the next change", () => {
      const replacement = vi.fn();
      let unsubscribe = () => {};
      const listener = vi.fn(() => {
        unsubscribe();
        store.subscribe(replacement);
      });
      unsubscribe = store.subscribe(listener);

      store.markRunning();
      expect(listener).toHaveBeenCalledTimes(1);
      expect(replacement).not.toHaveBeenCalled();

      store.markLoaded();
      expect(replacement).toHaveBeenCalledTimes(1);
    });

    it("getSnapshot returns a stable reference until state changes", () => {
      const snap1 = store.getSnapshot();
      const snap2 = store.getSnapshot();
      expect(snap1).toBe(snap2);

      store.handleEvent({ type: "token", content: "a" });
      const snap3 = store.getSnapshot();
      expect(snap3).not.toBe(snap1);
    });
  });

  describe("token events", () => {
    it("accumulates streaming text", () => {
      store.handleEvent({ type: "token", content: "Hello" });
      store.handleEvent({ type: "token", content: " world" });

      expect(store.streamingText).toBe("Hello world");
      expect(store.isRunning).toBe(true);
    });

    it("produces a synthetic streaming message in getDisplayMessages", () => {
      store.handleEvent({ type: "token", content: "Hello" });

      const msgs = store.getDisplayMessages();
      expect(msgs).toHaveLength(1);
      expect(msgs[0].id).toBe("__streaming__");
      expect(msgs[0].role).toBe("agent");
      expect(msgs[0].content).toBe("Hello");
      expect(msgs[0].status).toBe("executing");
    });
  });

  describe("reasoning events", () => {
    it("accumulates streaming reasoning", () => {
      store.handleEvent({ type: "reasoning", content: "Let me think" });
      store.handleEvent({ type: "reasoning", content: "..." });

      expect(store.streamingReasoning).toBe("Let me think...");
    });

    it("includes reasoning in the synthetic message", () => {
      store.handleEvent({ type: "reasoning", content: "thinking" });

      const msgs = store.getDisplayMessages();
      expect(msgs[0].reasoning).toBe("thinking");
    });
  });

  describe("tool_call events", () => {
    it("creates a streaming tool call entry", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: '{"query":"test"}',
        description: "Searching the web",
      });

      expect(store.streamingToolCalls.size).toBe(1);
      const tc = store.streamingToolCalls.get("te-1")!;
      expect(tc.toolName).toBe("web_search");
      expect(tc.args.query).toBe("test");
      expect(tc.args.description).toBe("Searching the web");
    });

    it("captures turn text from streaming text before the tool call", () => {
      store.handleEvent({ type: "token", content: "I'll search for that." });
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });

      const tc = store.streamingToolCalls.get("te-1")!;
      expect(tc.args.turnText).toBe("I'll search for that.");
    });

    it("keeps streamed text visible when a tool call starts", () => {
      store.handleEvent({ type: "token", content: "I'll search for that." });
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });

      expect(store.getDisplayMessages()[0].content).toBe("I'll search for that.");
    });

    it("starts post-tool streaming text on its own Markdown line", () => {
      store.handleEvent({ type: "token", content: "I'll search for that." });
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });
      store.handleEvent({ type: "tool_result", name: "web_search", success: true, summary: "Done" });
      store.handleEvent({ type: "token", content: "Here are the results." });

      expect(store.getDisplayMessages()[0].content).toBe(
        "I'll search for that.\n\nHere are the results.",
      );
    });

    it("shows tool executions in the synthetic message", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });

      const msgs = store.getDisplayMessages();
      expect(msgs[0].tool_calls).toHaveLength(1);
      expect(msgs[0].tool_calls![0].name).toBe("web_search");
      expect(msgs[0].tool_calls![0].result).toBe("");
    });

    it("handles object arguments (not just string)", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "cli",
        arguments: { command: "ls" },
      });

      const tc = store.streamingToolCalls.get("te-1")!;
      expect(tc.args.command).toBe("ls");
    });
  });

  describe("tool_result events", () => {
    it("matches result to the first unresolved tool call by name", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });
      store.handleEvent({
        type: "tool_result",
        name: "web_search",
        success: true,
        summary: "3 results found",
      });

      expect(store.streamingToolResults.size).toBe(1);
      const result = store.streamingToolResults.get("te-1")!;
      expect(result.result).toBe("3 results found");
      expect(result.isError).toBe(false);
    });

    it("marks failed results", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "cli",
        arguments: "{}",
      });
      store.handleEvent({
        type: "tool_result",
        name: "cli",
        success: false,
        summary: "Command failed",
      });

      const result = store.streamingToolResults.get("te-1")!;
      expect(result.isError).toBe(true);
    });

    it("uses 'Done' as default summary when none provided", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });
      store.handleEvent({
        type: "tool_result",
        name: "web_search",
        success: true,
      });

      const result = store.streamingToolResults.get("te-1")!;
      expect(result.result).toBe("Done");
    });

    it("matches multiple tool calls of the same name in order", () => {
      store.handleEvent({ type: "tool_call", id: "te-1", provider_call_id: "tc-1", name: "cli", arguments: "{}" });
      store.handleEvent({ type: "tool_call", id: "te-2", provider_call_id: "tc-2", name: "cli", arguments: "{}" });

      store.handleEvent({ type: "tool_result", name: "cli", success: true, summary: "first" });
      store.handleEvent({ type: "tool_result", name: "cli", success: true, summary: "second" });

      expect(store.streamingToolResults.get("te-1")!.result).toBe("first");
      expect(store.streamingToolResults.get("te-2")!.result).toBe("second");
    });
  });

  describe("inference_done event", () => {
    it("finalizes the message and clears streaming state", () => {
      store.handleEvent({ type: "token", content: "Hello" });
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });
      store.handleEvent({
        type: "tool_result",
        name: "web_search",
        success: true,
        summary: "Done",
      });

      const finalMsg = makeAgentMessage({
        id: "msg-final",
        content: "Hello",
        status: "completed",
        tool_calls: [makeToolCall({ id: "te-1", provider_call_id: "tc-1", name: "web_search" })],
      });

      store.handleEvent({ type: "inference_done", message: finalMsg });

      expect(store.isRunning).toBe(false);
      expect(store.streamingText).toBe("");
      expect(store.streamingToolCalls.size).toBe(0);
      expect(store.messages).toHaveLength(1);
      expect(store.messages[0].id).toBe("msg-final");
      expect(store.messages[0].status).toBe("completed");
    });

    it("merges tool executions from streaming and final message (dedup by id)", () => {
      store.handleEvent({
        type: "tool_call",
        id: "te-1",
        provider_call_id: "tc-1",
        name: "web_search",
        arguments: "{}",
      });
      store.handleEvent({
        type: "tool_result",
        name: "web_search",
        success: true,
        summary: "Done",
      });

      const finalMsg = makeAgentMessage({
        id: "msg-final",
        tool_calls: [makeToolCall({ id: "tc-1", provider_call_id: "tc-1" })],
      });

      store.handleEvent({ type: "inference_done", message: finalMsg });

      // Should not duplicate — tc-1 from streaming and tc-1 from final are deduped
      expect(store.messages[0].tool_calls).toHaveLength(1);
    });

    it("updates existing message in place if id matches", () => {
      store.messages.push(makeAgentMessage({ id: "msg-1", content: "old", status: "executing" }));

      store.handleEvent({
        type: "inference_done",
        message: makeAgentMessage({ id: "msg-1", content: "new", status: "completed" }),
      });

      expect(store.messages).toHaveLength(1);
      expect(store.messages[0].content).toBe("new");
    });

    // Reproduces a bug where after the agent loop pauses and re-pauses on a
    // NEW set of HITLs, `clearStreaming()` wiped `pendingExternalTools` and
    // the new pending tool calls — which arrive on `msg.tool_calls` — were
    // never re-seeded into the map. The wizard then saw an empty set and
    // never showed the new questions.
    it("re-seeds pendingExternalTools from msg.tool_calls on pause inference_done", () => {
      const pausedMsg = makeAgentMessage({
        id: "msg-1",
        status: "executing",
        tool_calls: [
          makeToolCall({
            id: "new-te-1",
            name: "ask_user_question",
            hitl: {
              prompt: "Q?",
              url: "/chats/c1",
              request: { type: "Question", data: { options: ["a", "b"] } },
              status: "pending",
              response: null,
              delivery: null,
            },
          }),
          makeToolCall({
            id: "new-te-2",
            name: "ask_user_question",
            hitl: {
              prompt: "Q2?",
              url: "/chats/c1",
              request: { type: "Question", data: { options: ["x"] } },
              status: "pending",
              response: null,
              delivery: null,
            },
          }),
        ],
      });

      store.handleEvent({ type: "inference_done", message: pausedMsg });

      const pending = store.getPendingExternalTools();
      expect(pending.map((t) => t.id).sort()).toEqual(["new-te-1", "new-te-2"]);
    });
  });

  describe("chat_message event", () => {
    it("keeps persisted task messages chronological when SSE events arrive out of order", () => {
      const completion = makeAgentMessage({
        id: "msg-completion",
        agent_id: "researcher",
        content: "Marked the task as completed.",
        created_at: "2026-09-04T01:44:01Z",
      });
      const prompt = makeAgentMessage({
        id: "msg-prompt",
        agent_id: "dark-matter",
        content: "Research the latest official US Apple MacBook prices.",
        created_at: "2026-09-04T01:44:00Z",
      });

      store.handleEvent({ type: "inference_done", message: completion });
      store.handleEvent({ type: "chat_message", message: prompt });

      expect(store.getDisplayMessages().map((message) => message.id)).toEqual([
        "msg-prompt",
        "msg-completion",
      ]);
    });

    it("replaces optimistic user message", () => {
      store.addUserMessage("Hello");
      expect(store.messages[0].id).toMatch(/^__user_/);

      const realMsg = makeUserMessage({ id: "msg-real" });
      store.handleEvent({ type: "chat_message", message: realMsg });

      expect(store.messages).toHaveLength(1);
      expect(store.messages[0].id).toBe("msg-real");
    });

    it("appends non-user messages", () => {
      const agentMsg = makeAgentMessage({ id: "msg-agent" });
      store.handleEvent({ type: "chat_message", message: agentMsg });

      expect(store.messages).toHaveLength(1);
      expect(store.messages[0].id).toBe("msg-agent");
    });

    it("skips duplicate message ids", () => {
      store.messages.push(makeAgentMessage({ id: "msg-1" }));
      store.handleEvent({ type: "chat_message", message: makeAgentMessage({ id: "msg-1" }) });

      expect(store.messages).toHaveLength(1);
    });
  });

  describe("inference_paused event", () => {
    it("stores external tools from msg.tool_calls into pendingExternalTools", () => {
      const te = makeToolCall({
        id: "te-ext",
        provider_call_id: "tc-ext",
        name: "ask_user_question",
        message_id: "msg-ext",
        hitl: {
          prompt: "Continue?",
          url: "/chats/c1",
          request: { type: "Question", data: { options: ["Yes", "No"] } },
          status: "pending",
          response: null,
          delivery: null,
        },
      });

      store.handleEvent({
        type: "inference_paused",
        reason: { type: "Hitl" },
        message: makeAgentMessage({ id: "msg-ext", status: "executing", tool_calls: [te] }),
      });

      expect(store.getPendingExternalTools()).toHaveLength(1);
      expect(store.getPendingExternalTools()[0].id).toBe("te-ext");
      expect(store.isRunning).toBe(false);
    });
  });

  describe("inference_resume event", () => {
    it("updates message when full message is provided", () => {
      const original = makeAgentMessage({ id: "msg-1", content: "old", status: "executing" });
      store.messages.push(original);

      const updated = makeAgentMessage({ id: "msg-1", content: "resolved", status: "completed" });
      store.handleEvent({ type: "inference_resume", message: updated });

      expect(store.messages[0].content).toBe("resolved");
      expect(store.messages[0].status).toBe("completed");
    });

    it("preserves existing tool_calls when message has none", () => {
      const te = makeToolCall({ id: "te-1" });
      store.messages.push(makeAgentMessage({ id: "msg-1", tool_calls: [te], status: "executing" }));

      const updatedMsg = makeAgentMessage({ id: "msg-1", tool_calls: [], status: "completed" });
      store.handleEvent({ type: "inference_resume", message: updatedMsg });

      expect(store.messages[0].tool_calls).toHaveLength(1);
    });
  });

  describe("retry event", () => {
    it("sets retry info", () => {
      store.handleEvent({ type: "retry", retryAfterSecs: 5, reason: "rate_limited" });

      expect(store.retryInfo).not.toBeNull();
      expect(store.retryInfo!.reason).toBe("rate_limited");
      expect(store.retryInfo!.retryAfterSecs).toBe(5);
    });

    it("is included in snapshot", () => {
      store.handleEvent({ type: "retry", retryAfterSecs: 5, reason: "rate_limited" });

      const snap = store.getSnapshot();
      expect(snap.retryInfo).not.toBeNull();
      expect(snap.retryInfo!.reason).toBe("rate_limited");
    });
  });

  describe("inference_cancelled / inference_error", () => {
    it("clears streaming state on cancellation", () => {
      store.handleEvent({ type: "token", content: "partial" });
      store.handleEvent({ type: "inference_cancelled", reason: "user cancelled" });

      expect(store.isRunning).toBe(false);
      expect(store.streamingText).toBe("");
    });

    it("clears streaming state on error", () => {
      store.handleEvent({ type: "token", content: "partial" });
      store.handleEvent({ type: "inference_error", error: "model error" });

      expect(store.isRunning).toBe(false);
      expect(store.streamingText).toBe("");
    });
  });

  describe("addUserMessage", () => {
    it("adds optimistic user message and sets isRunning", () => {
      store.addUserMessage("Hello");

      expect(store.messages).toHaveLength(1);
      expect(store.messages[0].id).toMatch(/^__user_/);
      expect(store.messages[0].role).toBe("user");
      expect(store.messages[0].content).toBe("Hello");
      expect(store.isRunning).toBe(true);
    });
  });

  describe("clearStreaming", () => {
    it("resets all streaming state", () => {
      store.handleEvent({ type: "token", content: "text" });
      store.handleEvent({ type: "reasoning", content: "reason" });
      store.handleEvent({ type: "tool_call", id: "te-1", provider_call_id: "tc-1", name: "test", arguments: "{}" });
      store.handleEvent({ type: "retry", retryAfterSecs: 5, reason: "test" });

      store.clearStreaming();

      expect(store.isRunning).toBe(false);
      expect(store.streamingText).toBe("");
      expect(store.streamingReasoning).toBe("");
      expect(store.streamingToolCalls.size).toBe(0);
      expect(store.streamingToolResults.size).toBe(0);
      expect(store.retryInfo).toBeNull();
    });
  });

  describe("getDisplayMessages with existing executing message", () => {
    it("merges new streaming text into an existing executing message", () => {
      store.messages.push(
        makeAgentMessage({
          id: "msg-1",
          content: "existing",
          status: "executing",
          reasoning: "prior reasoning",
        }),
      );
      store.isRunning = true;

      store.handleEvent({ type: "token", content: " new text" });
      store.handleEvent({ type: "reasoning", content: " more thinking" });

      const msgs = store.getDisplayMessages();
      expect(msgs).toHaveLength(1);
      expect(msgs[0].id).toBe("msg-1");
      expect(msgs[0].content).toBe("existing new text");
      expect(msgs[0].reasoning).toBe("prior reasoning more thinking");
    });

    it("merges new streaming tool calls into an existing executing message", () => {
      store.messages.push(
        makeAgentMessage({
          id: "msg-1",
          content: "existing",
          status: "executing",
          tool_calls: [makeToolCall({ id: "te-existing" })],
        }),
      );
      store.isRunning = true;

      store.handleEvent({ type: "tool_call", id: "te-new", provider_call_id: "tc-new", name: "cli", arguments: "{}" });

      const msgs = store.getDisplayMessages();
      expect(msgs).toHaveLength(1);
      expect(msgs[0].id).toBe("msg-1");
      expect(msgs[0].tool_calls!.length).toBe(2);
    });

    it("does not duplicate a persisted tool call when SSE replays it", () => {
      const toolCallId = "01a06ba9-4121-7a16-8562-d4113fc0a944";
      store.messages.push(
        makeAgentMessage({
          id: "msg-1",
          status: "executing",
          tool_calls: [makeToolCall({ id: toolCallId })],
        }),
      );

      store.handleEvent({
        type: "tool_call",
        id: toolCallId,
        provider_call_id: "tc-replayed",
        name: "web_search",
        arguments: "{}",
      });

      const toolCallIds = store.getDisplayMessages()[0].tool_calls!.map((tool) => tool.id);
      expect(toolCallIds).toEqual([toolCallId]);
    });
  });

  describe("resolveToolCall", () => {
    it("resolves a tool call within a message", () => {
      const te = makeToolCall({
        id: "te-1",
        provider_call_id: "tc-1",
        hitl: {
          prompt: "?",
          url: "/chats/c1",
          request: { type: "Question", data: { options: ["A"] } },
          status: "pending",
          response: null,
          delivery: null,
        },
      });
      store.messages.push(makeAgentMessage({ id: "msg-1", tool_calls: [te], status: "executing" }));

      store.resolveToolCall("tc-1", "A");

      const updated = store.messages[0].tool_calls![0];
      expect(updated.result).toBe("A");
      expect(updated.hitl!.status).toBe("resolved");
      // All tools resolved → status should flip to completed
      expect(store.messages[0].status).toBe("completed");
    });
  });
});

// ---------------------------------------------------------------------------
// mergeConsecutiveMessages
// ---------------------------------------------------------------------------

describe("mergeConsecutiveMessages", () => {
  it("marks consecutive agent messages from the same agent as continuations", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({ id: "1", agent_id: "a1", content: "First" }),
      makeAgentMessage({ id: "2", agent_id: "a1", content: "Second" }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
    expect((result[0] as any)._continuation).toBeUndefined();
    expect((result[1] as any)._continuation).toBe(true);
  });

  it("does not merge messages from different agents", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({ id: "1", agent_id: "a1", content: "Agent 1" }),
      makeAgentMessage({ id: "2", agent_id: "a2", content: "Agent 2" }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
  });

  it("does not merge when previous message is executing", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({ id: "1", agent_id: "a1", content: "Running", status: "executing" }),
      makeAgentMessage({ id: "2", agent_id: "a1", content: "Next" }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
  });

  it("does not merge user + agent messages", () => {
    const msgs: MessageResponse[] = [
      makeUserMessage({ id: "1" }),
      makeAgentMessage({ id: "2" }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
  });

  it("marks system event messages after agent messages as continuations", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({ id: "1", agent_id: "a1", content: "Agent says" }),
      {
        id: "2",
        chat_id: "chat-1",
        role: "system",
        content: "Task completed",
        event: { type: "TaskCompletion", data: { task_id: "t1", chat_id: null, status: "completed" } },
        created_at: "2026-01-01T00:00:00Z",
      },
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
    expect((result[1] as any)._continuation).toBe(true);
  });

  it("marks consecutive messages with tool executions as continuations", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({
        id: "1",
        agent_id: "a1",
        content: "",
        tool_calls: [makeToolCall({ id: "te-1" })],
      }),
      makeAgentMessage({
        id: "2",
        agent_id: "a1",
        content: "Done",
        tool_calls: [makeToolCall({ id: "te-2" })],
      }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
    expect((result[1] as any)._continuation).toBe(true);
  });

  it("preserves individual message status on continuations", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({ id: "1", agent_id: "a1", content: "A", status: "completed" }),
      makeAgentMessage({ id: "2", agent_id: "a1", content: "B", status: "failed" }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
    expect(result[0].status).toBe("completed");
    expect(result[1].status).toBe("failed");
  });

  it("marks consecutive messages with reasoning as continuations", () => {
    const msgs: MessageResponse[] = [
      makeAgentMessage({ id: "1", agent_id: "a1", content: "", reasoning: "First thought" }),
      makeAgentMessage({ id: "2", agent_id: "a1", content: "", reasoning: "Second thought" }),
    ];

    const result = mergeConsecutiveMessages(msgs);
    expect(result).toHaveLength(2);
    expect((result[1] as any)._continuation).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Full streaming sequence
// ---------------------------------------------------------------------------

describe("ChatStore: full streaming sequence", () => {
  it("handles a complete text + tool + result + done flow", () => {
    const store = new ChatStore();

    // User message echo
    store.handleEvent({
      type: "chat_message",
      message: makeUserMessage({ id: "user-1", content: "Search for cats" }),
    });

    // Streaming tokens
    store.handleEvent({ type: "token", content: "I'll search" });
    store.handleEvent({ type: "token", content: " for that." });

    // Tool call
    store.handleEvent({
      type: "tool_call",
      id: "te-1",
      provider_call_id: "tc-1",
      name: "web_search",
      arguments: '{"query":"cats"}',
      description: "Searching for cats",
    });

    // At this point, display should show user msg + synthetic streaming msg
    let display = store.getDisplayMessages();
    expect(display).toHaveLength(2);
    expect(display[1].status).toBe("executing");
    expect(display[1].tool_calls).toHaveLength(1);

    // Tool result
    store.handleEvent({
      type: "tool_result",
      name: "web_search",
      success: true,
      summary: "Found 10 results",
    });

    // More tokens after tool
    store.handleEvent({ type: "token", content: "Here are the results." });

    // inference_done
    const finalMsg = makeAgentMessage({
      id: "msg-agent-1",
      content: "I'll search for that.\n\nHere are the results.",
      status: "completed",
      tool_calls: [
        makeToolCall({
          id: "tc-1",
          provider_call_id: "tc-1",
          name: "web_search",
          arguments: { query: "cats" },
          result: "Found 10 results",
          description: "Searching for cats",
          turn_text: "I'll search for that.",
        }),
      ],
    });

    store.handleEvent({ type: "inference_done", message: finalMsg });

    display = store.getDisplayMessages();
    expect(display).toHaveLength(2); // user + agent
    expect(display[1].id).toBe("msg-agent-1");
    expect(display[1].status).toBe("completed");
    expect(display[1].tool_calls).toHaveLength(1);
    expect(store.isRunning).toBe(false);
  });

  it("handles multiple tool calls in sequence", () => {
    const store = new ChatStore();

    store.handleEvent({ type: "tool_call", id: "te-1", provider_call_id: "tc-1", name: "web_search", arguments: "{}" });
    store.handleEvent({ type: "tool_result", name: "web_search", success: true, summary: "Done" });
    store.handleEvent({ type: "tool_call", id: "te-2", provider_call_id: "tc-2", name: "cli", arguments: "{}" });
    store.handleEvent({ type: "tool_result", name: "cli", success: true, summary: "OK" });
    store.handleEvent({ type: "token", content: "All done." });

    const display = store.getDisplayMessages();
    expect(display).toHaveLength(1);
    expect(display[0].tool_calls).toHaveLength(2);
    expect(display[0].content).toBe("All done.");
  });
});
