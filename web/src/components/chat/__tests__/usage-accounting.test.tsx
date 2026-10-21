import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { ChatStore } from "@/lib/chat-store";
import type { UsageRecorded } from "@/lib/sse-event-bus";
import { UsagePill, PerMessageUsageFooter } from "../usage-pill";

const api = vi.hoisted(() => ({ get: vi.fn() }));
vi.mock("@/lib/api-client", () => ({ api }));

const usage: UsageRecorded = {
  chat_id: "chat-1", user_id: "user-1", agent_id: "agent-1", message_id: "msg-1",
  kind_tag: "Chat", model_ref: "custom-native-or-gateway", input_tokens: 1000,
  cached_input_tokens: 600, output_tokens: 100, cost_usd: 0.02,
  duration_ms: 500, fallback_index: 0, total_tokens: 1100,
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("ResizeObserver", class {
    observe() {} unobserve() {} disconnect() {}
  });
});

async function openTooltip(container: HTMLElement) {
  fireEvent.focus(container.querySelector('[tabindex="0"]')!);
  return within(await screen.findByRole("tooltip"));
}

it("renders the same 60% cache reads from new usage and recovered historical totals", async () => {
  const store = new ChatStore();
  store.handleEvent({ type: "usage_recorded", usage });
  expect(store.usagePerChat).toMatchObject({ inputTokens: 1000, cachedInputTokens: 600, incompleteUsageCalls: 0, costUsd: 0.02 });
  expect(store.lastChatInputTokens).toBe(1000);
  const view = render(<UsagePill totals={store.usagePerChat} />);
  let tooltip = await openTooltip(view.container);
  expect(tooltip.getByText("Cached 600 · 60%")).toBeInTheDocument();
  expect(tooltip.getByText("Other input 400")).toBeInTheDocument();
  expect(tooltip.getByText(/includes fresh input and cache writes/)).toBeInTheDocument();
  view.unmount();

  api.get.mockResolvedValue({ totals: {
    input_tokens: 1000, cached_input_tokens: 600, output_tokens: 100,
    cost_usd: 0.02, calls: 1, incomplete_usage_calls: 0,
  }, last_chat_input_tokens: 1000, total_tool_calls: 0 });
  const reloaded = new ChatStore();
  await reloaded.loadUsageSeed("chat-1");
  expect(reloaded.usagePerChat).toEqual(store.usagePerChat);
  const reloadView = render(<UsagePill totals={reloaded.usagePerChat} />);
  tooltip = await openTooltip(reloadView.container);
  expect(tooltip.getByText("Cached 600 · 60%")).toBeInTheDocument();
  expect(tooltip.getByText("$0.02")).toBeInTheDocument();
});

it.each([undefined, 1100])("keeps incomplete and new SSE costs/counts without a misleading ratio (total %s)", async (total_tokens) => {
  const store = new ChatStore();
  store.handleEvent({ type: "usage_recorded", usage: { ...usage, total_tokens, input_tokens: 200 } });
  store.handleEvent({ type: "usage_recorded", usage });
  expect(store.usagePerChat).toMatchObject({ inputTokens: 1200, cachedInputTokens: 1200, calls: 2, costUsd: 0.04, incompleteUsageCalls: 1 });
  const view = render(<UsagePill totals={store.usagePerChat} />);
  const tooltip = await openTooltip(view.container);
  expect(tooltip.getByText(/Input counts are incomplete/)).toBeInTheDocument();
  expect(tooltip.queryByText(/100%/)).not.toBeInTheDocument();
  expect(tooltip.queryByText(/Other input/)).not.toBeInTheDocument();
  view.unmount();
  render(<PerMessageUsageFooter totals={store.usageByMessage.get("msg-1")} />);
  expect(screen.getByText(/input counts incomplete/)).toHaveTextContent("$0.04");
});

it.each([undefined, 1])("preserves incomplete seed accounting when new events arrive (marker %s)", async (incomplete_usage_calls) => {
  api.get.mockResolvedValue({ totals: {
    input_tokens: 200, cached_input_tokens: 600, output_tokens: 100,
    cost_usd: 0.02, calls: 1, incomplete_usage_calls,
  }, last_chat_input_tokens: 200, total_tool_calls: 0 });
  const store = new ChatStore();
  await store.loadUsageSeed("chat-1");
  store.handleEvent({ type: "usage_recorded", usage });
  expect(store.usagePerChat).toMatchObject({ inputTokens: 1200, cachedInputTokens: 1200, calls: 2, costUsd: 0.04, incompleteUsageCalls: 1 });
  const view = render(<UsagePill totals={store.usagePerChat} />);
  const tooltip = await openTooltip(view.container);
  expect(tooltip.getByText(/Cache ratio unavailable/)).toBeInTheDocument();
  store.resetUsage();
  expect(store.usagePerChat.incompleteUsageCalls).toBe(0);
});


it.each([
  { name: "missing total", patch: { total_tokens: undefined } },
  { name: "total mismatch", patch: { total_tokens: 900 } },
  { name: "reads exceeding input", patch: { cached_input_tokens: 1200 } },
  { name: "negative reads", patch: { cached_input_tokens: -1 } },
  { name: "fractional counts", patch: { input_tokens: 1000.5, total_tokens: 1100.5 } },
])("withholds ratios for $name even when the event otherwise looks normalized", async ({ patch }) => {
  const store = new ChatStore();
  store.handleEvent({ type: "usage_recorded", usage: { ...usage, ...patch } });
  expect(store.usagePerChat.incompleteUsageCalls).toBe(1);
  const view = render(<UsagePill totals={store.usagePerChat} />);
  const tooltip = await openTooltip(view.container);
  expect(tooltip.getByText(/Input counts are incomplete/)).toBeInTheDocument();
  expect(tooltip.queryByText(/60%/)).not.toBeInTheDocument();
  expect(tooltip.getByText("$0.02")).toBeInTheDocument();
});
