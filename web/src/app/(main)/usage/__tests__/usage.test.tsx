import type { ReactNode } from "react";
import { render, screen, within } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({ get: vi.fn() }));
vi.mock("@/lib/api-client", () => ({ api }));
const user = { id: "user-1" };
vi.mock("@/lib/auth", () => ({ useAuth: () => ({ user }) }));
vi.mock("@/lib/navigation-context", () => ({ useNavigation: () => ({ standaloneChats: [], archivedChats: [] }) }));
// Keep the production provider, page calculations, labels and summaries. Replace
// only canvas/layout concerns with inspectable chart inputs in jsdom.
vi.mock("recharts", () => {
  const Container = ({ children }: { children?: ReactNode }) => <div>{children}</div>;
  const Chart = ({ data, children }: { data: unknown; children?: ReactNode }) => (
    <div data-testid="chart" data-series={JSON.stringify(data)}>{children}</div>
  );
  const Series = ({ dataKey, name }: { dataKey: string; name?: string }) => (
    <span data-testid="series" data-key={dataKey}>{name ?? dataKey}</span>
  );
  return {
    ResponsiveContainer: Container, AreaChart: Chart, LineChart: Chart,
    ComposedChart: Chart, BarChart: Chart, PieChart: Container,
    Area: Series, Line: Series, Bar: Series, Pie: Container,
    CartesianGrid: () => null, Cell: () => null, XAxis: () => null,
    YAxis: () => null, Tooltip: () => null,
  };
});

import { UsageProvider, type UsageRollup } from "../usage-context";
import Overview from "../page";
import Tokens from "../tokens/page";
import Cost from "../cost/page";

const normalized: UsageRollup = {
  input_tokens: 1000, cached_input_tokens: 600, output_tokens: 100,
  calls: 1, cost_usd: 0.02, incomplete_usage_calls: 0,
};

function respond(totals: UsageRollup, series = [totals]) {
  api.get.mockResolvedValue({
    totals, by_model: { "custom-native-or-gateway": totals }, by_kind: { Chat: totals },
    series: series.map((row, i) => ({ ...row, bucket: `2026-10-${20 + i}T00:00:00Z` })),
  });
}

function chartData(index = 0) {
  return JSON.parse(screen.getAllByTestId("chart")[index].getAttribute("data-series")!);
}

beforeEach(() => vi.clearAllMocks());

it.each([Overview, Tokens])("shows recovered historical totals and normalized cache reads through the real usage fetch in %s", async (Page) => {
  respond(normalized);
  render(<UsageProvider><Page /></UsageProvider>);
  await screen.findByText("Other input");
  expect(api.get).toHaveBeenCalledWith(expect.stringMatching(/^\/api\/users\/user-1\/usage\?/));
  expect(screen.getByText(Page === Tokens ? "60% of input" : "60% cached")).toBeVisible();
  expect(chartData()[0]).toMatchObject({ input: 1000, cached: 600, otherInput: 400, output: 100 });
  expect(screen.getByText(/fresh input and cache writes/)).toBeVisible();
  expect(screen.queryByText(/Some usage records have/)).not.toBeInTheDocument();
});

it.each([undefined, 1])("preserves historical totals and cost while suppressing incomplete ratios (marker %s)", async (incomplete_usage_calls) => {
  // Invalid input can be smaller than its cache reads.
  respond({ ...normalized, input_tokens: 200, incomplete_usage_calls });
  render(<UsageProvider><Overview /></UsageProvider>);
  expect(await screen.findByText("Cache ratio unavailable")).toBeVisible();
  expect(screen.getByText("200 / 100")).toBeVisible();
  expect(screen.getAllByText("$0.020").length).toBeGreaterThan(0);
  expect(screen.getByText(/Recorded totals and costs are retained/)).toBeVisible();
  expect(screen.getByText("Recorded input")).toBeVisible();
  expect(screen.queryByText("Other input")).not.toBeInTheDocument();
  expect(screen.queryByText(/300%/)).not.toBeInTheDocument();
});

it("omits only incomplete time buckets and model ratios from mixed-period cache effectiveness", async () => {
  const incomplete = { ...normalized, input_tokens: 200, incomplete_usage_calls: 1 };
  respond({ ...normalized, input_tokens: 1200, cached_input_tokens: 1200, calls: 2, incomplete_usage_calls: 1 }, [incomplete, normalized]);
  render(<UsageProvider><Tokens /></UsageProvider>);
  await screen.findByText("Recorded input");
  expect(chartData(1).map((row: { ratio: number | null }) => row.ratio)).toEqual([null, 60]);
  expect(screen.getByText(/cache ratio unavailable\)/)).toBeVisible();
  expect(screen.queryByText(/100% cached/)).not.toBeInTheDocument();
});

it("shows an unavailable cache chart for wholly incomplete data", async () => {
  respond({ ...normalized, incomplete_usage_calls: 1 });
  render(<UsageProvider><Tokens /></UsageProvider>);
  expect(await screen.findAllByText("Cache ratio unavailable")).toHaveLength(2);
});

it.each([0, undefined])("handles empty usage without an unavailable warning (completeness %s)", async (incomplete_usage_calls) => {
  respond({ ...normalized, input_tokens: 0, cached_input_tokens: 0, output_tokens: 0, calls: 0, incomplete_usage_calls });
  render(<UsageProvider><Tokens /></UsageProvider>);
  await screen.findByText("Other input");
  expect(screen.getByText("0% of input")).toBeVisible();
  expect(chartData(1)[0].ratio).toBe(0);
  expect(document.body.textContent).not.toMatch(/NaN|Infinity/);
  expect(screen.queryByText(/Some usage records have/)).not.toBeInTheDocument();
});

it.each([0, 1])("retains total spend and request pricing, qualifies effective token pricing (incomplete %s)", async (incomplete_usage_calls) => {
  respond({ ...normalized, incomplete_usage_calls });
  render(<UsageProvider><Cost /></UsageProvider>);
  const card = (await screen.findByText("Cost / 1M tokens")).parentElement!;
  await screen.findByText("custom-native-or-gateway");
  expect(within(card).getByText(incomplete_usage_calls ? "Unavailable" : "$18.18")).toBeVisible();
  expect(screen.getAllByText("$0.020").length).toBeGreaterThan(0);
});
