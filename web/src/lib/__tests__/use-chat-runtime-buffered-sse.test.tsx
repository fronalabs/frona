import { render, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { ChatView } from "@/components/chat/conversation-panel";
import { sseBus } from "../sse-event-bus";

const mocks = vi.hoisted(() => ({
  apiGet: vi.fn(),
  getPendingMessage: vi.fn(() => null),
}));

class ResizeObserverStub implements ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

vi.stubGlobal("ResizeObserver", ResizeObserverStub);
Object.defineProperty(HTMLElement.prototype, "scrollTo", {
  configurable: true,
  value: vi.fn(),
});

vi.mock("../api-client", () => ({
  api: {
    get: (...args: unknown[]) => mocks.apiGet(...args),
    post: vi.fn(),
  },
  cancelGeneration: vi.fn(),
  sendMessage: vi.fn(),
  uploadFile: vi.fn(),
}));

vi.mock("../format-time", () => ({
  computeTimeMarkers: () => new Map(),
  useTimezone: () => "UTC",
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: vi.fn() }),
}));

vi.mock("@/lib/session-context", () => ({
  useSession: () => ({
    setActiveChat: vi.fn(),
    getPendingMessage: mocks.getPendingMessage,
  }),
}));

vi.mock("@/lib/navigation-context", () => ({
  useNavigation: () => ({ addStandaloneChat: vi.fn() }),
}));

vi.mock("@/lib/activity-context", () => ({
  useActivity: () => ({
    snapshot: { executions: [] },
    loading: false,
    stale: false,
    refresh: vi.fn(),
  }),
}));

vi.mock("@/components/chat/tool-uis", () => ({
  ToolUIRegistry: () => null,
}));

vi.mock("@/components/chat/chat-header", () => ({
  ChatHeader: ({ totalToolCalls }: { totalToolCalls: number }) => (
    <div data-testid="tool-count">{totalToolCalls}</div>
  ),
}));

vi.mock("@/components/chat/frona-user-message", () => ({
  FronaUserMessage: () => <div data-testid="user-message" />,
}));

vi.mock("@/components/chat/frona-assistant-message", () => ({
  FronaAssistantMessage: () => <div data-testid="assistant-message" />,
}));

vi.mock("@/components/chat/frona-composer", () => ({
  FronaComposer: () => <div data-testid="composer" />,
}));

describe("useChatRuntime buffered SSE replay", () => {
  beforeEach(() => {
    mocks.apiGet.mockImplementation((path: string) => {
      if (path.endsWith("/usage")) {
        return Promise.resolve({
          totals: {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0,
            calls: 0,
          },
          last_chat_input_tokens: null,
          total_tool_calls: 0,
        });
      }
      return Promise.resolve({ messages: [], has_more: false });
    });
  });

  it("opens a chat after its SSE tool events were buffered", async () => {
    const chatId = `buffered-chat-${crypto.randomUUID()}`;

    sseBus.routeEvent("inference_start", chatId, {});
    for (let index = 0; index < 30; index += 1) {
      sseBus.routeEvent("tool_call", chatId, {
        id: `tool-${index}`,
        provider_call_id: `provider-tool-${index}`,
        name: `tool_${index}`,
        arguments: { index },
      });
      sseBus.routeEvent("tool_result", chatId, {
        name: `tool_${index}`,
        success: true,
        summary: `result ${index}`,
      });
    }

    const view = render(
      <ChatView chatId={chatId} agentId="agent-1" />,
    );

    await waitFor(() => {
      expect(view.getByTestId("composer")).toBeInTheDocument();
      expect(view.getByTestId("tool-count")).toHaveTextContent("30");
    });

    view.unmount();
  });

  it("replays a buffered tool call already present in the loaded executing message", async () => {
    const chatId = `replayed-tool-chat-${crypto.randomUUID()}`;
    const toolCallId = "01a06ba9-4121-7a16-8562-d4113fc0a944";

    mocks.apiGet.mockImplementation((path: string) => {
      if (path.endsWith("/usage")) {
        return Promise.resolve({
          totals: {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0,
            calls: 0,
          },
          last_chat_input_tokens: null,
          total_tool_calls: 0,
        });
      }
      return Promise.resolve({
        messages: [{
          id: "message-1",
          chat_id: chatId,
          role: "agent",
          content: "",
          status: "executing",
          created_at: "2026-09-04T01:44:00Z",
          tool_calls: [{
            id: toolCallId,
            chat_id: chatId,
            message_id: "message-1",
            turn: 1,
            provider_call_id: "provider-tool-1",
            name: "web_search",
            arguments: {},
            result: "",
            success: true,
            duration_ms: 0,
            created_at: "2026-09-04T01:44:00Z",
          }],
        }],
        has_more: false,
      });
    });

    sseBus.routeEvent("tool_call", chatId, {
      id: toolCallId,
      provider_call_id: "provider-tool-1",
      name: "web_search",
      arguments: {},
    });

    const view = render(<ChatView chatId={chatId} agentId="agent-1" />);

    await waitFor(() => {
      expect(view.getByTestId("composer")).toBeInTheDocument();
      expect(view.getAllByTestId("assistant-message")).toHaveLength(1);
    });

    view.unmount();
  });
});
