"use client";

import type { ReactNode } from "react";

export function fmtUsd(n: number): string {
  if (n === 0) return "$0";
  if (n < 0.01) return `$${n.toFixed(4)}`;
  if (n < 1) return `$${n.toFixed(3)}`;
  return `$${n.toFixed(2)}`;
}

export function fmtK(n: number): string {
  if (n < 1000) return n.toString();
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

export function fmtMs(n: number | null | undefined): string {
  if (n == null) return "—";
  if (n < 1000) return `${Math.round(n)}ms`;
  return `${(n / 1000).toFixed(1)}s`;
}

export function fmtBucketLabel(iso: string, bucket: "hour" | "day"): string {
  const dt = new Date(iso);
  return bucket === "hour"
    ? dt.toLocaleTimeString(undefined, { hour: "numeric" })
    : dt.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className={`rounded-lg border border-border bg-surface p-4 ${className ?? ""}`}>
      {children}
    </div>
  );
}

export function SectionHeader({
  title,
  subtitle,
  right,
}: {
  title: string;
  subtitle?: string;
  right?: ReactNode;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <div>
        <h2 className="text-base font-semibold text-text-primary">{title}</h2>
        {subtitle && <p className="mt-0.5 text-sm text-text-tertiary">{subtitle}</p>}
      </div>
      {right}
    </div>
  );
}

export function SummaryCard({
  label,
  value,
  hint,
  loading,
}: {
  label: string;
  value: string;
  hint?: string;
  loading?: boolean;
}) {
  return (
    <Card>
      <p className="text-xs uppercase tracking-wide text-text-tertiary">{label}</p>
      <p className="mt-2 text-2xl font-semibold tabular-nums text-text-primary">
        {loading ? "…" : value}
      </p>
      {hint && <p className="mt-1 text-xs text-text-tertiary">{hint}</p>}
    </Card>
  );
}

export function EmptyState({ loading }: { loading: boolean }) {
  return (
    <div className="flex h-full w-full items-center justify-center">
      <p className="text-sm text-text-tertiary">
        {loading ? "Loading…" : "No data in this window"}
      </p>
    </div>
  );
}

export const CHART_PALETTE = [
  "var(--accent)",
  "var(--text-secondary)",
  "var(--text-tertiary)",
  "var(--border)",
  "var(--surface-tertiary)",
];

export const CHART_TOOLTIP_STYLE = {
  background: "var(--surface)",
  border: "1px solid var(--border)",
  borderRadius: 8,
  fontSize: 12,
};

export function IncompleteUsageNote() {
  return (
    <p className="mb-4 text-sm text-text-tertiary">
      Some usage records have incomplete input counts. Recorded totals and costs are retained;
      cache percentages and input breakdowns are unavailable for those records.
    </p>
  );
}
