import { makeMessageError } from "./fixtures/message-error";
import { describe, it, expect } from "vitest";
import { appendTurnText, convertMessage, type AssistantContentPart } from "../use-chat-runtime";
import type { MessageResponse, ToolCall } from "../types";


function makeAgentMessage(overrides: Partial<MessageResponse> = {}): MessageResponse {
  return {
    id: "msg-1",
    chat_id: "chat-1",
    role: "agent",
    content: "Hello",
    status: "completed",
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

it("marks a saved failed reply as an error rather than a completed response", () => {
  expect(convertMessage(makeAgentMessage({ status: "failed", content: "" }))?.status)
    .toEqual({ type: "incomplete", reason: "error", error: "Message processing failed." });
});

it("preserves the saved provider error and partial reply", () => {
  const message = convertMessage(makeAgentMessage({
    content: "Partial reply",
    status: "failed",
    error: makeMessageError("This model is not supported for your account."),
  }));
  expect(message?.status).toEqual({ type: "incomplete", reason: "error", error: makeMessageError("This model is not supported for your account.") });
  expect(message?.content).toContainEqual({ type: "text", text: "Partial reply" });
});


describe("convertMessage: user messages", () => {
  it("converts a basic user message", () => {
    const msg: MessageResponse = {
      id: "msg-u1",
      chat_id: "chat-1",
      role: "user",
      content: "Hello",
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result).not.toBeNull();
    expect(result!.role).toBe("user");
    expect(result!.content).toEqual([{ type: "text", text: "Hello" }]);
    expect(result!.id).toBe("msg-u1");
  });

  it("converts a contact message as user role", () => {
    const msg: MessageResponse = {
      id: "msg-c1",
      chat_id: "chat-1",
      role: "contact",
      content: "Hi from contact",
      contact_id: "contact-1",
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result!.role).toBe("user");
    expect(result!.metadata.custom.originalRole).toBe("contact");
    expect(result!.metadata.custom.contactId).toBe("contact-1");
  });

  it("converts a livecall message as user role", () => {
    const msg: MessageResponse = {
      id: "msg-lc1",
      chat_id: "chat-1",
      role: "livecall",
      content: "Voice input",
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result!.role).toBe("user");
    expect(result!.metadata.custom.originalRole).toBe("livecall");
  });
});


describe("convertMessage: agent messages", () => {
  it("converts a basic agent message", () => {
    const msg = makeAgentMessage({ content: "Response text" });

    const result = convertMessage(msg);
    expect(result!.role).toBe("assistant");
    expect(result!.content).toEqual(
      expect.arrayContaining([{ type: "text", text: "Response text" }]),
    );
  });

  it("exposes reasoning text via metadata for the header toggle", () => {
    const msg = makeAgentMessage({ reasoning: "Let me think about this" });

    const result = convertMessage(msg);
    expect((result!.metadata.custom as Record<string, unknown>).reasoning).toBe("Let me think about this");
    expect(result!.content.find((p: any) => p.type === "reasoning")).toBeUndefined();
  });

  it("preserves agent_id in metadata", () => {
    const msg = makeAgentMessage({ agent_id: "researcher" });

    const result = convertMessage(msg);
    expect((result!.metadata.custom as Record<string, unknown>).agentId).toBe("researcher");
  });

  it("adds empty text part when no content and no reasoning", () => {
    const msg = makeAgentMessage({ content: "", reasoning: undefined });

    const result = convertMessage(msg);
    expect(result!.content).toEqual(
      expect.arrayContaining([{ type: "text", text: "" }]),
    );
  });
});


describe("convertMessage: status mapping", () => {
  it("maps executing status to running", () => {
    const msg = makeAgentMessage({ status: "executing" });

    const result = convertMessage(msg);
    expect(result!.status).toEqual({ type: "running" });
  });

  it("maps completed status to complete/stop", () => {
    const msg = makeAgentMessage({ status: "completed" });

    const result = convertMessage(msg);
    expect(result!.status).toEqual({ type: "complete", reason: "stop" });
  });

  it("maps pending hitl to requires-action", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          hitl: {
            prompt: "?",
            url: "/chats/c1",
            request: { type: "Question", data: { options: ["A"] } },
            status: "pending",
            response: null,
            delivery: null,
          },
        }),
      ],
    });

    const result = convertMessage(msg);
    expect(result!.status).toEqual({ type: "requires-action", reason: "tool-calls" });
  });

  it("maps resolved hitl to complete", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          hitl: {
            prompt: "?",
            url: "/chats/c1",
            request: { type: "Question", data: { options: ["A"] } },
            status: "resolved",
            response: { type: "Choice", data: "A" },
            delivery: null,
          },
        }),
      ],
    });

    const result = convertMessage(msg);
    expect(result!.status).toEqual({ type: "complete", reason: "stop" });
  });
});


describe("convertMessage: tool executions", () => {
  it("converts regular tool executions to tool-call parts", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          id: "te-1",
          name: "web_search",
          arguments: { query: "test" },
          result: "Found it",
          description: "Searching",
        }),
      ],
    });

    const result = convertMessage(msg);
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(toolPart).toBeDefined();
    expect(toolPart.toolCallId).toBe("te-1");
    expect(toolPart.toolName).toBe("web_search");
    expect(toolPart.args.description).toBe("Searching");
    expect(toolPart.result).toBe("Found it");
  });

  it("converts hitl executions using the request type as toolName", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          id: "te-q1",
          hitl: {
            prompt: "Pick one",
            url: "/chats/c1",
            request: { type: "Question", data: { options: ["A", "B"] } },
            status: "resolved",
            response: { type: "Choice", data: "A" },
            delivery: null,
          },
        }),
      ],
    });

    const result = convertMessage(msg);
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(toolPart.toolName).toBe("Question");
    expect(toolPart.toolCallId).toBe("te-q1");
    expect(toolPart.result).toBe("A");
  });

  it("hitl with pending status has no result", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          hitl: {
            prompt: "Check this",
            url: "/chats/c1",
            request: { type: "Takeover", data: { reason: "Check this", debugger_url: "http://..." } },
            status: "pending",
            response: null,
            delivery: null,
          },
        }),
      ],
    });

    const result = convertMessage(msg);
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(toolPart.result).toBeUndefined();
  });

  it("hitl with denied status uses 'denied' as result", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          hitl: {
            prompt: "Allow access?",
            url: "/chats/c1",
            request: { type: "Credential", data: { query: "creds", reason: "need auth" } },
            status: "denied",
            response: null,
            delivery: null,
          },
        }),
      ],
    });

    const result = convertMessage(msg);
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(toolPart.result).toBe("denied");
  });

  it("renders turn_text before completed message text", () => {
    const msg = makeAgentMessage({
      tool_calls: [
        makeToolCall({
          turn_text: "Before the tool",
        }),
      ],
    });

    const result = convertMessage(msg);
    const textPart = result!.content.find((p: any) => p.type === "text") as any;
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(textPart.text).toBe("Before the tool\n\nHello");
    expect(toolPart.args.turnText).toBeUndefined();
  });

  it("surfaces the agent's turn text when body is empty and reasoning is present", () => {
    // The reported bug: an internal-tool turn (memory_remember) where the agent
    // streamed text into turn_text, the message body is empty, and reasoning is
    // set - the empty-text placeholder is suppressed, so the words must be
    // recovered from the tool call or they vanish behind "Used 1 tool".
    const msg = makeAgentMessage({
      content: "",
      reasoning: "Mina told me something personal.",
      tool_calls: [
        makeToolCall({
          name: "memory_remember",
          turn_text: "Oh nice, that's a big shop. What team are you on?",
        }),
      ],
    });

    const result = convertMessage(msg);
    const textPart = result!.content.find((p: any) => p.type === "text") as any;
    expect(textPart?.text).toBe("Oh nice, that's a big shop. What team are you on?");
    expect(result!.content[0].type).toBe("text");
  });
});


describe("convertMessage: special roles", () => {
  it("converts taskcompletion messages as assistant", () => {
    const msg: MessageResponse = {
      id: "msg-tc1",
      chat_id: "chat-1",
      role: "taskcompletion",
      content: "Task done",
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result!.role).toBe("assistant");
    expect(result!.metadata.custom.originalRole).toBe("taskcompletion");
  });

  it("converts system message with event as assistant", () => {
    const msg: MessageResponse = {
      id: "msg-sys1",
      chat_id: "chat-1",
      role: "system",
      content: "Task completed",
      event: { type: "TaskCompletion", data: { task_id: "t1", chat_id: null, status: "completed" } },
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result!.role).toBe("assistant");
  });

  it("returns null for system messages without events", () => {
    const msg: MessageResponse = {
      id: "msg-sys2",
      chat_id: "chat-1",
      role: "system",
      content: "Internal system message",
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result).toBeNull();
  });

  it("suppresses taskcompletion bubble when content is empty and not failed", () => {
    const msg: MessageResponse = {
      id: "msg-tc-signal",
      chat_id: "chat-1",
      role: "taskcompletion",
      content: "",
      event: { type: "TaskCompletion", data: { task_id: "t1", chat_id: null, status: "Completed" } },
      created_at: "2026-01-01T00:00:00Z",
    };

    expect(convertMessage(msg)).toBeNull();
  });

  it("shows failed taskcompletion even with empty content", () => {
    const msg: MessageResponse = {
      id: "msg-tc-fail",
      chat_id: "chat-1",
      role: "taskcompletion",
      content: "",
      event: { type: "TaskCompletion", data: { task_id: "t1", chat_id: null, status: "Failed" } },
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result).not.toBeNull();
    expect(result!.role).toBe("assistant");
  });

  it("shows taskcompletion with result content", () => {
    const msg: MessageResponse = {
      id: "msg-tc-result",
      chat_id: "chat-1",
      role: "taskcompletion",
      content: "# Research Findings\n\nHere are the results...",
      event: { type: "TaskCompletion", data: { task_id: "t1", chat_id: null, status: "Completed" } },
      created_at: "2026-01-01T00:00:00Z",
    };

    const result = convertMessage(msg);
    expect(result).not.toBeNull();
    expect(result!.role).toBe("assistant");
  });
});


describe("appendTurnText", () => {
  it("puts turnText before an existing final text part", () => {
    const parts: AssistantContentPart[] = [
      { type: "text", text: "Already here" },
      { type: "tool-call", toolCallId: "tc-1", toolName: "cli", args: { turnText: "Before tool" } as any, argsText: "{}", result: "ok" },
    ];

    const result = appendTurnText(parts);
    expect(result[0]).toEqual({ type: "text", text: "Before tool\n\nAlready here" });
    expect((result[1] as any).args.turnText).toBeUndefined();
  });

  it("promotes last turnText to text part when text is empty", () => {
    const parts: AssistantContentPart[] = [
      { type: "text", text: "" },
      { type: "tool-call", toolCallId: "tc-1", toolName: "cli", args: { turnText: "First turn" } as any, argsText: "{}", result: "ok" },
      { type: "tool-call", toolCallId: "tc-2", toolName: "cli", args: { turnText: "Last turn" } as any, argsText: "{}", result: "ok" },
    ];

    const result = appendTurnText(parts);
    expect((result[0] as any).text).toBe("First turn\n\nLast turn");
    expect((result[1] as any).args.turnText).toBeUndefined();
    expect((result[2] as any).args.turnText).toBeUndefined();
  });

  it("does nothing when no turnText exists", () => {
    const parts: AssistantContentPart[] = [
      { type: "text", text: "" },
      { type: "tool-call", toolCallId: "tc-1", toolName: "cli", args: {} as any, argsText: "{}", result: "ok" },
    ];

    const result = appendTurnText(parts);
    expect((result[0] as any).text).toBe("");
  });

  it("replaces whitespace-only text with turnText", () => {
    const parts: AssistantContentPart[] = [
      { type: "text", text: "   " },
      { type: "tool-call", toolCallId: "tc-1", toolName: "cli", args: { turnText: "Before" } as any, argsText: "{}", result: "ok" },
    ];

    const result = appendTurnText(parts);
    expect((result[0] as any).text).toBe("Before");
  });

  it("preserves reasoning parts", () => {
    const parts: AssistantContentPart[] = [
      { type: "reasoning", text: "thinking" },
      { type: "text", text: "" },
      { type: "tool-call", toolCallId: "tc-1", toolName: "cli", args: { turnText: "Before" } as any, argsText: "{}", result: "ok" },
    ];

    const result = appendTurnText(parts);
    expect(result[0]).toEqual({ type: "reasoning", text: "thinking" });
  });

  it("prepends a text part when turnText exists but no text part is present", () => {
    // The internal-tool case: reasoning suppressed the empty-text placeholder,
    // so content is a lone tool-call whose turnText carries the agent's words.
    const parts: AssistantContentPart[] = [
      { type: "tool-call", toolCallId: "tc-1", toolName: "memory_remember", args: { turnText: "Oh nice, big shop." } as any, argsText: "{}", result: "ok" },
    ];

    const result = appendTurnText(parts);
    expect(result[0]).toEqual({ type: "text", text: "Oh nice, big shop." });
    expect((result[1] as any).args.turnText).toBeUndefined();
  });
});


describe("convertMessage: turnText appending gated on status", () => {
  it("removes duplicate turnText from executing tool-call args", () => {
    const msg = makeAgentMessage({
      content: "",
      status: "executing",
      tool_calls: [
        makeToolCall({ turn_text: "I'll do that" }),
      ],
    });

    const result = convertMessage(msg);
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(toolPart.args.turnText).toBeUndefined();
  });

  it("appends turnText on completed messages", () => {
    const msg = makeAgentMessage({
      content: "",
      status: "completed",
      tool_calls: [
        makeToolCall({ turn_text: "I'll do that" }),
      ],
    });

    const result = convertMessage(msg);
    const textPart = result!.content.find((p: any) => p.type === "text") as any;
    expect(textPart.text).toBe("I'll do that");
    const toolPart = result!.content.find((p: any) => p.type === "tool-call") as any;
    expect(toolPart.args.turnText).toBeUndefined();
  });
});
