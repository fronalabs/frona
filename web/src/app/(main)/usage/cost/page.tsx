"use client";

import { useMemo } from "react";
import Link from "next/link";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip as RTooltip,
  XAxis,
  YAxis,
} from "recharts";

import { useNavigation } from "@/lib/navigation-context";
import { hasIncompleteUsage, useUsage } from "../usage-context";
import {
  CHART_PALETTE,
  CHART_TOOLTIP_STYLE,
  Card,
  EmptyState,
  IncompleteUsageNote,
  SectionHeader,
  SummaryCard,
  fmtBucketLabel,
  fmtK,
  fmtUsd,
} from "../widgets";

export default function CostPage() {
  const { range, data, loading } = useUsage();
  const { standaloneChats, archivedChats } = useNavigation();

  const chatTitleById = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of standaloneChats) m.set(c.id, c.title ?? "Untitled chat");
    for (const c of archivedChats) m.set(c.id, c.title ?? "Untitled chat");
    return m;
  }, [standaloneChats, archivedChats]);

  const costSeries = useMemo(() => {
    if (!data?.series) return [];
    let acc = 0;
    return data.series.map((b) => {
      acc += b.cost_usd;
      return {
        label: fmtBucketLabel(b.bucket, range.bucket),
        cost: b.cost_usd,
        cumulative: acc,
      };
    });
  }, [data, range]);

  const byKind = useMemo(() => {
    if (!data) return [];
    return Object.entries(data.by_kind)
      .map(([name, r]) => ({ name, value: r.cost_usd, calls: r.calls }))
      .filter((r) => r.value > 0)
      .sort((a, b) => b.value - a.value);
  }, [data]);

  const byModel = useMemo(() => {
    if (!data) return [];
    return Object.entries(data.by_model)
      .map(([name, r]) => ({
        name,
        cost: r.cost_usd,
        calls: r.calls,
        cost_per_call: r.calls > 0 ? r.cost_usd / r.calls : 0,
      }))
      .filter((r) => r.cost > 0)
      .sort((a, b) => b.cost - a.cost);
  }, [data]);

  const incomplete = data ? hasIncompleteUsage(data.totals) : false;

  const avgCostPerCall =
    data && data.totals.calls > 0 ? data.totals.cost_usd / data.totals.calls : 0;
  const avgCostPerMillion =
    data && data.totals.input_tokens + data.totals.output_tokens > 0
      ? (data.totals.cost_usd /
          (data.totals.input_tokens + data.totals.output_tokens)) *
        1_000_000
      : 0;

  return (
    <div className="mx-auto max-w-6xl px-4 py-6 md:px-8 md:py-10">
      <div className="mb-6">
        <h1 className="text-2xl font-semibold text-text-primary">Cost</h1>
        <p className="text-sm text-text-tertiary">
          Where your spend goes — over time, by kind, by model, by chat.
        </p>
      </div>

      {incomplete && <IncompleteUsageNote />}

      <div className="mb-6 grid grid-cols-2 gap-3 md:grid-cols-4">
        <SummaryCard label="Total spend" value={fmtUsd(data?.totals.cost_usd ?? 0)} loading={loading && !data} />
        <SummaryCard label="Avg / request" value={fmtUsd(avgCostPerCall)} loading={loading && !data} />
        <SummaryCard
          label="Cost / 1M tokens"
          value={incomplete ? "Unavailable" : fmtUsd(avgCostPerMillion)}
          hint={incomplete ? "Input counts incomplete" : "input + output blended"}
          loading={loading && !data}
        />
        <SummaryCard label="API requests" value={(data?.totals.calls ?? 0).toLocaleString()} loading={loading && !data} />
      </div>

      <Card className="mb-6">
        <SectionHeader title="Spend over time" subtitle="Per-bucket cost with cumulative line." />
        <div className="mt-4 h-72">
          {costSeries.length === 0 ? (
            <EmptyState loading={loading} />
          ) : (
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={costSeries} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
                <CartesianGrid stroke="var(--border)" strokeOpacity={0.5} vertical={false} />
                <XAxis dataKey="label" stroke="var(--text-tertiary)" fontSize={11} tickLine={false} axisLine={false} />
                <YAxis stroke="var(--text-tertiary)" fontSize={11} tickLine={false} axisLine={false} tickFormatter={fmtUsd} />
                <RTooltip contentStyle={CHART_TOOLTIP_STYLE} formatter={((v: number) => fmtUsd(v)) as never} />
                <Bar dataKey="cost" fill="var(--accent)" radius={[3, 3, 0, 0]} isAnimationActive={false} />
              </BarChart>
            </ResponsiveContainer>
          )}
        </div>
      </Card>

      <div className="mb-6 grid grid-cols-1 gap-3 md:grid-cols-2">
        <Card>
          <SectionHeader title="By kind" subtitle="Cost share per inference kind." />
          <div className="mt-4 flex items-center gap-4">
            <div className="h-44 w-44 shrink-0">
              {byKind.length === 0 ? (
                <EmptyState loading={loading} />
              ) : (
                <ResponsiveContainer width="100%" height="100%">
                  <PieChart>
                    <Pie data={byKind} dataKey="value" nameKey="name" innerRadius={40} outerRadius={75} paddingAngle={2} stroke="var(--surface)" isAnimationActive={false}>
                      {byKind.map((_, i) => (
                        <Cell key={i} fill={CHART_PALETTE[i % CHART_PALETTE.length]} />
                      ))}
                    </Pie>
                    <RTooltip contentStyle={CHART_TOOLTIP_STYLE} formatter={((v: number) => fmtUsd(v)) as never} />
                  </PieChart>
                </ResponsiveContainer>
              )}
            </div>
            <ul className="flex-1 space-y-1.5 text-sm">
              {byKind.map((k, i) => (
                <li key={k.name} className="flex items-center justify-between gap-3">
                  <span className="flex items-center gap-2 truncate">
                    <span className="h-2 w-2 shrink-0 rounded-sm" style={{ background: CHART_PALETTE[i % CHART_PALETTE.length] }} />
                    <span className="truncate text-text-secondary">{k.name}</span>
                  </span>
                  <span className="tabular-nums text-text-primary">{fmtUsd(k.value)}</span>
                </li>
              ))}
            </ul>
          </div>
        </Card>

        <Card>
          <SectionHeader title="By model" subtitle="Cost and average cost per request." />
          {byModel.length === 0 ? (
            <div className="mt-4 h-44">
              <EmptyState loading={loading} />
            </div>
          ) : (
            <div className="mt-4 space-y-2">
              {byModel.map((m) => {
                const max = byModel[0].cost;
                return (
                  <div key={m.name}>
                    <div className="mb-1 flex items-baseline justify-between gap-2 text-sm">
                      <span className="truncate text-text-secondary">{m.name}</span>
                      <span className="tabular-nums text-text-primary">
                        {fmtUsd(m.cost)}
                        <span className="ml-2 text-xs text-text-tertiary">
                          ({fmtUsd(m.cost_per_call)}/req)
                        </span>
                      </span>
                    </div>
                    <div className="h-1.5 w-full rounded-full bg-border">
                      <div
                        className="h-full rounded-full bg-accent"
                        style={{ width: `${(m.cost / max) * 100}%` }}
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </Card>
      </div>

      <Card>
        <SectionHeader title="Top sessions by cost" subtitle="Click through to see each." />
        {(data?.top_chats?.length ?? 0) > 0 ? (
          <div className="mt-4 divide-y divide-border">
            {data!.top_chats!.map((c) => (
              <Link
                key={c.chat_id}
                href={`/chat?id=${c.chat_id}`}
                className="flex items-center justify-between gap-3 rounded px-2 py-2 transition hover:bg-surface-secondary"
              >
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm text-text-primary">
                    {chatTitleById.get(c.chat_id) ?? c.chat_id.slice(0, 8)}
                  </div>
                  <div className="text-xs text-text-tertiary">
                    {c.calls} API requests · {fmtK(c.input_tokens)}↑ {fmtK(c.output_tokens)}↓
                  </div>
                </div>
                <div className="tabular-nums text-sm text-text-primary">{fmtUsd(c.cost_usd)}</div>
              </Link>
            ))}
          </div>
        ) : (
          <p className="mt-4 text-sm text-text-tertiary">
            {loading ? "Loading…" : "No sessions with recorded usage in this window"}
          </p>
        )}
      </Card>
    </div>
  );
}
