"use client";

import { ArrowDownIcon, ArrowUpIcon } from "@heroicons/react/24/outline";
import * as Tooltip from "@radix-ui/react-tooltip";

import type { RunningTotals } from "@/lib/chat-store";

interface UsagePillProps {
  totals: RunningTotals;
  fallbackIndex?: number;
  currentInputTokens?: number;
  contextWindow?: number;
  totalToolCalls?: number;
}

function fmtK(n: number): string {
  if (n < 1000) return n.toString();
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

function fmtUsd(n: number): string {
  if (n === 0) return "$0";
  if (n < 0.01) return `$${n.toFixed(4)}`;
  return `$${n.toFixed(2)}`;
}

function ProgressBar({
  value,
  max,
  className,
}: {
  value: number;
  max: number;
  className: string;
}) {
  const pct = max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return (
    <div className="relative h-1.5 w-full overflow-hidden rounded-full bg-border">
      <div className={`absolute inset-y-0 left-0 ${className}`} style={{ width: `${pct}%` }} />
    </div>
  );
}

function StatRow({
  label,
  value,
  hint,
  emphasize = false,
}: {
  label: React.ReactNode;
  value: React.ReactNode;
  hint?: React.ReactNode;
  emphasize?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-text-tertiary">{label}</span>
      <span
        className={`tabular-nums ${emphasize ? "font-medium text-text-primary" : "text-text-primary"}`}
      >
        {value}
        {hint && <span className="ml-2 text-text-tertiary">{hint}</span>}
      </span>
    </div>
  );
}

function TooltipPanel({
  totals,
  fallbackIndex,
  currentInputTokens,
  contextWindow,
  totalToolCalls,
}: UsagePillProps) {
  const showContext =
    typeof currentInputTokens === "number" &&
    typeof contextWindow === "number" &&
    contextWindow > 0;
  const ctxPct = showContext
    ? Math.min(100, (currentInputTokens! / contextWindow!) * 100)
    : 0;
  const ctxColor =
    ctxPct > 85 ? "bg-amber-500" : ctxPct > 60 ? "bg-yellow-500" : "bg-accent";

  const incomplete = (totals.incompleteUsageCalls ?? totals.calls) > 0;
  const cachedPctOfInput =
    totals.inputTokens > 0
      ? (totals.cachedInputTokens / totals.inputTokens) * 100
      : 0;

  return (
    <div className="flex w-64 flex-col divide-y divide-border text-xs text-text-secondary">
      {showContext && (
        <section className="flex flex-col gap-1.5 p-3">
          <div className="flex items-baseline justify-between">
            <span className="text-[11px] font-semibold uppercase tracking-wide text-text-tertiary">
              Context
            </span>
            <span className="tabular-nums font-medium text-text-primary">
              {ctxPct.toFixed(0)}%
            </span>
          </div>
          <ProgressBar
            value={currentInputTokens!}
            max={contextWindow!}
            className={ctxColor}
          />
          <div className="flex justify-between text-text-tertiary tabular-nums">
            <span>{currentInputTokens!.toLocaleString()} tokens</span>
            <span>of {contextWindow!.toLocaleString()}</span>
          </div>
        </section>
      )}

      <section className="flex flex-col gap-1.5 p-3">
        <StatRow
          label={
            <span className="inline-flex items-center gap-1">
              <ArrowUpIcon className="h-3 w-3" aria-hidden /> Input
            </span>
          }
          value={fmtK(totals.inputTokens)}
        />
        {!incomplete && totals.inputTokens > 0 && (
          <div className="flex flex-col gap-0.5">
            <div className="flex h-1.5 w-full overflow-hidden rounded-full bg-border">
              {totals.cachedInputTokens > 0 && (
                <div
                  className="bg-accent"
                  style={{ width: `${cachedPctOfInput}%` }}
                />
              )}
              <div
                className="bg-accent/30"
                style={{
                  width: `${((totals.inputTokens - totals.cachedInputTokens) / totals.inputTokens) * 100}%`,
                }}
              />
            </div>
            {totals.cachedInputTokens > 0 && (
              <div className="flex justify-between text-text-tertiary tabular-nums">
                <span className="inline-flex items-center gap-1">
                  <span className="h-1.5 w-1.5 rounded-full bg-accent" aria-hidden />
                  Cached {fmtK(totals.cachedInputTokens)} · {cachedPctOfInput.toFixed(0)}%
                </span>
                <span className="inline-flex items-center gap-1">
                  Other input {fmtK(totals.inputTokens - totals.cachedInputTokens)}
                  <span className="h-1.5 w-1.5 rounded-full bg-accent/30" aria-hidden />
                </span>
              </div>
            )}
            <p className="text-text-tertiary">Other input includes fresh input and cache writes.</p>
          </div>
        )}
        {incomplete && (
          <p className="text-text-tertiary">
            Input counts are incomplete. Cache ratio unavailable.
          </p>
        )}
      </section>

      <section className="p-3">
        <StatRow
          label={
            <span className="inline-flex items-center gap-1">
              <ArrowDownIcon className="h-3 w-3" aria-hidden /> Output
            </span>
          }
          value={fmtK(totals.outputTokens)}
        />
      </section>

      {totals.costUsd > 0 && (
        <section className="p-3">
          <StatRow label="Cost" value={fmtUsd(totals.costUsd)} emphasize />
        </section>
      )}

      <section className="flex flex-col gap-1 p-3 text-text-tertiary">
        <div className="flex items-baseline justify-between">
          <span>
            {totals.calls} inference call{totals.calls === 1 ? "" : "s"}
          </span>
          {typeof totalToolCalls === "number" && totalToolCalls > 0 && (
            <span>
              {totalToolCalls} tool call{totalToolCalls === 1 ? "" : "s"}
            </span>
          )}
        </div>
        {fallbackIndex !== undefined && fallbackIndex > 0 && (
          <div className="text-amber-500">⚠ last call used fallback #{fallbackIndex}</div>
        )}
      </section>
    </div>
  );
}

export function UsagePill(props: UsagePillProps) {
  const { totals, fallbackIndex, currentInputTokens, contextWindow } = props;
  if (totals.calls === 0) return null;

  const showContext =
    typeof currentInputTokens === "number" &&
    typeof contextWindow === "number" &&
    contextWindow > 0;
  const pct = showContext
    ? Math.min(100, (currentInputTokens! / contextWindow!) * 100)
    : 0;

  return (
    <Tooltip.Provider delayDuration={150}>
      <Tooltip.Root>
        <Tooltip.Trigger asChild>
          <span
            className="inline-flex cursor-default items-center gap-2 rounded-full bg-muted px-2 py-0.5 text-xs text-muted-foreground"
            tabIndex={0}
          >
            <span className="inline-flex items-center gap-0.5">
              <ArrowUpIcon className="h-3 w-3" aria-hidden />
              {fmtK(totals.inputTokens)}
              {totals.cachedInputTokens > 0 && (
                <span className="ml-1 text-text-tertiary">
                  ({fmtK(totals.cachedInputTokens)})
                </span>
              )}
            </span>
            <span className="inline-flex items-center gap-0.5">
              <ArrowDownIcon className="h-3 w-3" aria-hidden />
              {fmtK(totals.outputTokens)}
            </span>
            {totals.costUsd > 0 && (
              <>
                <span aria-hidden>·</span>
                <span>{fmtUsd(totals.costUsd)}</span>
              </>
            )}
            {showContext && (
              <>
                <span aria-hidden>·</span>
                <span>
                  {fmtK(currentInputTokens!)}/{fmtK(contextWindow!)} ({pct.toFixed(0)}%)
                </span>
              </>
            )}
            {fallbackIndex !== undefined && fallbackIndex > 0 && (
              <span className="text-amber-500">⚠</span>
            )}
          </span>
        </Tooltip.Trigger>
        <Tooltip.Portal>
          <Tooltip.Content
            side="bottom"
            align="end"
            sideOffset={6}
            className="z-50 rounded-lg border border-border bg-surface-secondary shadow-lg animate-in fade-in-0 zoom-in-95"
          >
            <TooltipPanel {...props} />
            <Tooltip.Arrow className="fill-surface-secondary" />
          </Tooltip.Content>
        </Tooltip.Portal>
      </Tooltip.Root>
    </Tooltip.Provider>
  );
}

interface PerMessageUsageFooterProps {
  totals: RunningTotals | undefined;
  modelRef?: string;
}

export function PerMessageUsageFooter({ totals, modelRef }: PerMessageUsageFooterProps) {
  if (!totals || totals.calls === 0) return null;
  return (
    <div className="mt-1 text-[10px] text-muted-foreground">
      {fmtK(totals.inputTokens)} in
      {totals.cachedInputTokens > 0 && <> · {fmtK(totals.cachedInputTokens)} cached</>}
      {" · "}{fmtK(totals.outputTokens)} out
      {" · "}{fmtUsd(totals.costUsd)}
      {modelRef && <> — {modelRef}</>}
      {(totals.incompleteUsageCalls ?? totals.calls) > 0 && <> · input counts incomplete</>}
    </div>
  );
}
