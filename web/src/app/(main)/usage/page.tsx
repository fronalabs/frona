"use client";

import { useMemo } from "react";
import Link from "next/link";
import {
  Area,
  AreaChart,
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
import { hasIncompleteUsage, useUsage } from "./usage-context";
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
  fmtMs,
  fmtUsd,
} from "./widgets";

export default function UsageOverviewPage() {
  const { range, data, loading, error } = useUsage();
  const { standaloneChats, archivedChats } = useNavigation();

  const incomplete = data ? hasIncompleteUsage(data.totals) : false;

  const chatTitleById = useMemo(() => {
    const m = new Map<string, string>();
    for (const c of standaloneChats) m.set(c.id, c.title ?? "Untitled chat");
    for (const c of archivedChats) m.set(c.id, c.title ?? "Untitled chat");
    return m;
  }, [standaloneChats, archivedChats]);

  const chartData = useMemo(() => {
    if (!data?.series) return [];
    return data.series.map((b) => ({
      label: fmtBucketLabel(b.bucket, range.bucket),
      cached: b.cached_input_tokens,
      input: b.input_tokens,
      otherInput: Math.max(0, b.input_tokens - b.cached_input_tokens),
      output: b.output_tokens,
    }));
  }, [data, range]);

  const byKind = useMemo(() => {
    if (!data) return [];
    return Object.entries(data.by_kind)
      .map(([name, r]) => ({ name, value: r.cost_usd }))
      .filter((r) => r.value > 0)
      .sort((a, b) => b.value - a.value);
  }, [data]);

  const cacheRatio =
    data && data.totals.input_tokens > 0
      ? (data.totals.cached_input_tokens / data.totals.input_tokens) * 100
      : 0;

  return (
    <div className="mx-auto max-w-6xl px-4 py-6 md:px-8 md:py-10">
      <div className="mb-6">
        <h1 className="text-2xl font-semibold text-text-primary">Overview</h1>
        <p className="text-sm text-text-tertiary">
          High-level summary. Use the left nav for deeper breakdowns.
        </p>
      </div>

      {error && (
        <Card className="mb-4 border-red-500/30 bg-red-500/5">
          <p className="text-sm text-red-500">{error}</p>
        </Card>
      )}

      {incomplete && <IncompleteUsageNote />}

      <div className="mb-6 grid grid-cols-2 gap-3 md:grid-cols-4">
        <SummaryCard
          label="Cost"
          value={fmtUsd(data?.totals.cost_usd ?? 0)}
          loading={loading && !data}
        />
        <SummaryCard
          label="API requests"
          value={(data?.totals.calls ?? 0).toLocaleString()}
          loading={loading && !data}
        />
        <SummaryCard
          label="Tokens (in/out)"
          value={`${fmtK(data?.totals.input_tokens ?? 0)} / ${fmtK(data?.totals.output_tokens ?? 0)}`}
          hint={incomplete ? "Cache ratio unavailable" : data && data.totals.input_tokens > 0 ? `${cacheRatio.toFixed(0)}% cached` : undefined}
          loading={loading && !data}
        />
        <SummaryCard
          label="Duration p95"
          value={fmtMs(data?.latency?.duration_ms_p95)}
          loading={loading && !data}
        />
      </div>

      <Card className="mb-6">
        <SectionHeader
          title="Tokens over time"
          subtitle={incomplete ? "Recorded input and output per bucket." : "Cache reads + other input (fresh input and cache writes), stacked under output."}
        />
        <div className="mt-4 h-60">
          {chartData.length === 0 ? (
            <EmptyState loading={loading} />
          ) : (
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={chartData} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
                <CartesianGrid stroke="var(--border)" strokeOpacity={0.5} vertical={false} />
                <XAxis dataKey="label" stroke="var(--text-tertiary)" fontSize={11} tickLine={false} axisLine={false} />
                <YAxis stroke="var(--text-tertiary)" fontSize={11} tickLine={false} axisLine={false} tickFormatter={fmtK} />
                <RTooltip contentStyle={CHART_TOOLTIP_STYLE} formatter={((v: number) => fmtK(v)) as never} />
                {!incomplete && <Area type="monotone" name="Cache reads" dataKey="cached" stackId="t" stroke="var(--accent)" fill="var(--accent)" fillOpacity={0.7} isAnimationActive={false} />}
                <Area type="monotone" dataKey={incomplete ? "input" : "otherInput"} name={incomplete ? "Recorded input" : "Other input"} stackId="t" stroke="var(--accent)" fill="var(--accent)" fillOpacity={0.3} isAnimationActive={false} />
                <Area type="monotone" dataKey="output" stackId="t" stroke="var(--text-tertiary)" fill="var(--text-tertiary)" fillOpacity={0.25} isAnimationActive={false} />
              </AreaChart>
            </ResponsiveContainer>
          )}
        </div>
      </Card>

      <div className="mb-6 grid grid-cols-1 gap-3 md:grid-cols-2">
        <Card>
          <SectionHeader title="Cost by kind" subtitle="Where the dollars went." />
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
          <SectionHeader title="Top sessions" subtitle="Most expensive in this window." />
          {(data?.top_chats?.length ?? 0) > 0 ? (
            <div className="mt-4 divide-y divide-border">
              {data!.top_chats!.slice(0, 5).map((c) => (
                <Link
                  key={c.chat_id}
                  href={`/chat?id=${c.chat_id}`}
                  className="flex items-center justify-between gap-3 rounded px-2 py-2 transition hover:bg-surface-secondary"
                >
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm text-text-primary">
                      {chatTitleById.get(c.chat_id) ?? c.chat_id.slice(0, 8)}
                    </div>
                    <div className="text-xs text-text-tertiary">{c.calls} API requests</div>
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
    </div>
  );
}
