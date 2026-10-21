"use client";

import { useMemo } from "react";
import {
  Area,
  AreaChart,
  CartesianGrid,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip as RTooltip,
  XAxis,
  YAxis,
} from "recharts";

import { hasIncompleteUsage, useUsage } from "../usage-context";
import {
  CHART_TOOLTIP_STYLE,
  Card,
  EmptyState,
  IncompleteUsageNote,
  SectionHeader,
  SummaryCard,
  fmtBucketLabel,
  fmtK,
} from "../widgets";

export default function TokensPage() {
  const { range, data, loading } = useUsage();

  const incomplete = data ? hasIncompleteUsage(data.totals) : false;

  const tokenSeries = useMemo(() => {
    if (!data?.series) return [];
    return data.series.map((b) => ({
      label: fmtBucketLabel(b.bucket, range.bucket),
      cached: b.cached_input_tokens,
      input: b.input_tokens,
      otherInput: Math.max(0, b.input_tokens - b.cached_input_tokens),
      output: b.output_tokens,
    }));
  }, [data, range]);

  const cacheRatioSeries = useMemo(() => {
    if (!data?.series) return [];
    return data.series.map((b) => ({
      label: fmtBucketLabel(b.bucket, range.bucket),
      ratio: hasIncompleteUsage(b) ? null :
        b.input_tokens > 0
          ? Number(((b.cached_input_tokens / b.input_tokens) * 100).toFixed(1))
          : 0,
    }));
  }, [data, range]);

  const byModel = useMemo(() => {
    if (!data) return [];
    return Object.entries(data.by_model)
      .map(([name, r]) => ({
        name,
        input: r.input_tokens,
        cached: r.cached_input_tokens,
        output: r.output_tokens,
        calls: r.calls,
        incomplete: hasIncompleteUsage(r),
      }))
      .filter((m) => m.input + m.output > 0)
      .sort((a, b) => b.input + b.output - (a.input + a.output));
  }, [data]);

  const cacheRatioTotal =
    data && data.totals.input_tokens > 0
      ? (data.totals.cached_input_tokens / data.totals.input_tokens) * 100
      : 0;

  const avgTokensPerCall =
    data && data.totals.calls > 0
      ? (data.totals.input_tokens + data.totals.output_tokens) / data.totals.calls
      : 0;

  return (
    <div className="mx-auto max-w-6xl px-4 py-6 md:px-8 md:py-10">
      <div className="mb-6">
        <h1 className="text-2xl font-semibold text-text-primary">Tokens</h1>
        <p className="text-sm text-text-tertiary">
          Input / output / cached breakdown over time, by model, and cache effectiveness.
        </p>
      </div>

      {incomplete && <IncompleteUsageNote />}

      <div className="mb-6 grid grid-cols-2 gap-3 md:grid-cols-4">
        <SummaryCard label="Input tokens" value={fmtK(data?.totals.input_tokens ?? 0)} loading={loading && !data} />
        <SummaryCard
          label="Cached"
          value={fmtK(data?.totals.cached_input_tokens ?? 0)}
          hint={incomplete ? "Cache ratio unavailable" : `${cacheRatioTotal.toFixed(0)}% of input`}
          loading={loading && !data}
        />
        <SummaryCard label="Output tokens" value={fmtK(data?.totals.output_tokens ?? 0)} loading={loading && !data} />
        <SummaryCard label="Avg / call" value={fmtK(Math.round(avgTokensPerCall))} loading={loading && !data} />
      </div>

      <Card className="mb-6">
        <SectionHeader
          title="Tokens over time"
          subtitle={incomplete ? "Recorded input and output per bucket." : "Cache reads + other input (fresh input and cache writes), stacked under output."}
        />
        <div className="mt-4 h-72">
          {tokenSeries.length === 0 ? (
            <EmptyState loading={loading} />
          ) : (
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={tokenSeries} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
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

      <Card className="mb-6">
        <SectionHeader
          title="Cache-hit ratio over time"
          subtitle="Percentage of input that was a cache read. Buckets with incomplete input counts are omitted."
        />
        <div className="mt-4 h-52">
          {cacheRatioSeries.length > 0 && cacheRatioSeries.every((b) => b.ratio === null) ? (
            <p className="text-sm text-text-tertiary">Cache ratio unavailable</p>
          ) : cacheRatioSeries.length === 0 ? (
            <EmptyState loading={loading} />
          ) : (
            <ResponsiveContainer width="100%" height="100%">
              <LineChart data={cacheRatioSeries} margin={{ top: 8, right: 12, left: 0, bottom: 0 }}>
                <CartesianGrid stroke="var(--border)" strokeOpacity={0.5} vertical={false} />
                <XAxis dataKey="label" stroke="var(--text-tertiary)" fontSize={11} tickLine={false} axisLine={false} />
                <YAxis
                  stroke="var(--text-tertiary)"
                  fontSize={11}
                  tickLine={false}
                  axisLine={false}
                  domain={[0, 100]}
                  tickFormatter={(v) => `${v}%`}
                />
                <RTooltip contentStyle={CHART_TOOLTIP_STYLE} formatter={((v: number) => `${v}%`) as never} />
                <Line type="monotone" dataKey="ratio" stroke="var(--accent)" strokeWidth={2} dot={false} isAnimationActive={false} />
              </LineChart>
            </ResponsiveContainer>
          )}
        </div>
      </Card>

      <Card>
        <SectionHeader title="By model" subtitle="Tokens routed to each model." />
        {byModel.length === 0 ? (
          <div className="mt-4 h-32">
            <EmptyState loading={loading} />
          </div>
        ) : (
          <div className="mt-4 space-y-3">
            {byModel.map((m) => {
              const total = m.input + m.output;
              const max = byModel[0].input + byModel[0].output;
              const cachePct = m.input > 0 ? (m.cached / m.input) * 100 : 0;
              return (
                <div key={m.name}>
                  <div className="mb-1 flex items-baseline justify-between gap-2 text-sm">
                    <span className="truncate text-text-secondary">{m.name}</span>
                    <span className="tabular-nums text-text-primary">
                      {fmtK(total)}
                      <span className="ml-2 text-xs text-text-tertiary">
                        ({fmtK(m.input)}↑ {fmtK(m.output)}↓ · {m.incomplete ? "cache ratio unavailable" : `${cachePct.toFixed(0)}% cached`})
                      </span>
                    </span>
                  </div>
                  <div className="h-1.5 w-full rounded-full bg-border">
                    <div
                      className="h-full rounded-full bg-accent"
                      style={{ width: `${(total / max) * 100}%` }}
                    />
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </Card>
    </div>
  );
}
