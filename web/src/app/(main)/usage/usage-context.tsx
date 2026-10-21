"use client";

import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";

import { api } from "@/lib/api-client";
import { useAuth } from "@/lib/auth";

export type UsageRollup = {
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  calls: number;
  incomplete_usage_calls?: number;
};

export type UsageBucket = {
  bucket: string;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  calls: number;
  incomplete_usage_calls?: number;
};

// Without completeness data, nonempty aggregates cannot support reliable ratios.
export function hasIncompleteUsage(usage: { calls: number; incomplete_usage_calls?: number }): boolean {
  return (usage.incomplete_usage_calls ?? usage.calls) > 0;
}

export type LatencyPercentiles = {
  duration_ms_p50: number | null;
  duration_ms_p95: number | null;
  duration_ms_p99: number | null;
  ttft_ms_p50: number | null;
  ttft_ms_p95: number | null;
  ttft_ms_p99: number | null;
};

export type LatencyBucket = LatencyPercentiles & { bucket: string };
export type ModelLatencyRow = LatencyPercentiles & { model_ref: string };

export type ChatCostRow = {
  chat_id: string;
  cost_usd: number;
  calls: number;
  input_tokens: number;
  output_tokens: number;
};

export interface UsageResponse {
  totals: UsageRollup;
  by_kind: Record<string, UsageRollup>;
  by_model: Record<string, UsageRollup>;
  series?: UsageBucket[];
  latency?: LatencyPercentiles;
  latency_by_model?: ModelLatencyRow[];
  latency_series?: LatencyBucket[];
  top_chats?: ChatCostRow[];
}

export const RANGES = [
  { label: "24h", hours: 24, bucket: "hour" as const },
  { label: "7d", hours: 24 * 7, bucket: "day" as const },
  { label: "30d", hours: 24 * 30, bucket: "day" as const },
  { label: "90d", hours: 24 * 90, bucket: "day" as const },
];

export type UsageRange = (typeof RANGES)[number];

interface Ctx {
  rangeIdx: number;
  setRangeIdx: (n: number) => void;
  range: UsageRange;
  data: UsageResponse | null;
  loading: boolean;
  error: string | null;
}

const UsageContext = createContext<Ctx | null>(null);

export function UsageProvider({ children }: { children: ReactNode }) {
  const { user } = useAuth();
  const [rangeIdx, setRangeIdx] = useState(2); // default 30d
  const [data, setData] = useState<UsageResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const range = RANGES[rangeIdx];

  useEffect(() => {
    if (!user) return;
    setLoading(true);
    setError(null);
    const since = new Date(Date.now() - range.hours * 60 * 60 * 1000).toISOString();
    const params = new URLSearchParams({
      since,
      bucket: range.bucket,
      top_chats: "20",
    });
    api
      .get<UsageResponse>(`/api/users/${user.id}/usage?${params}`)
      .then((d) => setData(d))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setLoading(false));
  }, [user, range]);

  const value = useMemo(
    () => ({ rangeIdx, setRangeIdx, range, data, loading, error }),
    [rangeIdx, range, data, loading, error],
  );

  return <UsageContext.Provider value={value}>{children}</UsageContext.Provider>;
}

export function useUsage(): Ctx {
  const ctx = useContext(UsageContext);
  if (!ctx) throw new Error("useUsage must be used within UsageProvider");
  return ctx;
}
