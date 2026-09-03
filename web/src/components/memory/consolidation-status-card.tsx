"use client";

import { useEffect, useState } from "react";
import { formatDistanceToNow } from "date-fns";
import { CheckCircleIcon, ChevronDownIcon, ExclamationTriangleIcon } from "@heroicons/react/24/outline";
import { getPkmStatus, type PkmConsolidationStatus } from "@/lib/api-client";

const stages = ["Ingest", "Classify", "Resolve", "Reconcile", "Assemble", "Resolve playbooks", "Build playbooks", "Build pages", "Cleanup"];

function number(value: number) {
  return new Intl.NumberFormat(undefined, { notation: value >= 10_000 ? "compact" : "standard", maximumFractionDigits: 1 }).format(value);
}

function stageName(stage: string) {
  return ({
    ingest: "Ingesting conversations", classify: "Classifying entities", resolve: "Resolving duplicate entities",
    reconcile: "Reconciling knowledge", assemble: "Checking facts", playbook_resolve: "Resolving playbooks",
    playbook_author: "Building playbooks", page_author: "Building memory pages", cleanup: "Cleaning up",
    done: "Consolidation complete", failed: "Consolidation failed",
  } as Record<string, string>)[stage] ?? stage;
}

export function ConsolidationStatusCard({
  value,
  className = "",
  compact = false,
  detailsOnly = false,
  expanded: controlledExpanded,
  onExpandedChange,
}: {
  value?: PkmConsolidationStatus | null;
  className?: string;
  compact?: boolean;
  detailsOnly?: boolean;
  expanded?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
}) {
  const [internalExpanded, setInternalExpanded] = useState(false);
  const expanded = controlledExpanded ?? internalExpanded;
  const controlled = value !== undefined;
  const [loaded, setLoaded] = useState<PkmConsolidationStatus | null>(value ?? null);
  const status = controlled ? value ?? null : loaded;

  useEffect(() => {
    if (controlled) return;
    let cancelled = false;
    const load = async () => {
      try {
        const response = await getPkmStatus();
        if (!cancelled) setLoaded(response.consolidation ?? null);
      } catch { /* The graph/settings availability UI owns request errors. */ }
    };
    void load();
    const interval = status?.status === "running" || status?.status === "retrying"
      ? window.setInterval(load, 2500) : undefined;
    return () => { cancelled = true; if (interval) window.clearInterval(interval); };
  }, [controlled, status?.status]);

  if (compact) {
    const active = status?.status === "running" || status?.status === "retrying";
    const toggleExpanded = () => {
      const next = !expanded;
      if (controlledExpanded === undefined) setInternalExpanded(next);
      onExpandedChange?.(next);
    };
    return (
      <div className={className}>
        <button type="button" onClick={toggleExpanded} className="flex h-[46px] items-center gap-2 rounded-xl border border-border bg-surface-secondary/95 px-3 text-sm text-text-secondary shadow-lg backdrop-blur hover:text-text-primary" aria-expanded={expanded} aria-label="Memory consolidation status">
          <span className={`h-2 w-2 rounded-full ${status?.status === "failed" ? "bg-danger" : active ? "animate-pulse bg-accent" : "bg-success"}`} />
          <span className="hidden whitespace-nowrap sm:inline">{status ? (active ? stageName(status.stage) : status.status === "failed" ? "Consolidation failed" : "Memory up to date") : "Memory consolidation"}</span>
          <ChevronDownIcon className={`h-4 w-4 transition ${expanded ? "rotate-180" : ""}`} />
        </button>
        {expanded && <ConsolidationStatusCard value={status} className="absolute right-0 mt-2 w-[min(520px,calc(100vw-24px))]" />}
      </div>
    );
  }

  if (!status) {
    return (
      <section className={`rounded-xl border border-border bg-surface-secondary p-4 ${className}`}>
        <h2 className="text-sm font-semibold text-text-primary">Memory consolidation</h2>
        <p className="mt-1 text-sm text-text-secondary">No consolidation has run yet. Frona will process eligible conversations automatically.</p>
      </section>
    );
  }

  const running = status.status === "running" || status.status === "retrying";
  const totalTokens = status.usage.input_tokens + status.usage.output_tokens;
  const summary = status.summary;
  const results = [
    ["Memories added", summary.memoriesAdded], ["Entities created", summary.entitiesCreated + summary.entitiesMinted],
    ["Entities merged", summary.entitiesMerged], ["Entities reconciled", summary.entitiesReconciled],
    ["Pages built", summary.pagesBuilt], ["Playbooks built", summary.playbooksBuilt],
    ["Grounding corrections", summary.groundingCorrections], ["Citation repairs", summary.citationRepairs],
    ["Duplicate claims skipped", summary.duplicateClaims], ["Unsupported claims dropped", summary.unsupportedClaims],
    ["Items cleaned", summary.itemsCleaned],
  ] as const;

  if (detailsOnly) {
    return (
      <div className={className}>
        {status.failure && (
          <p className={`mb-3 text-sm ${status.status === "failed" ? "text-danger" : "text-text-secondary"}`}>
            {status.failure.message}{status.failure.affectedCount > 0 && ` (${status.failure.affectedCount} affected items)`}
          </p>
        )}
        <div className="grid grid-cols-2 gap-x-6 gap-y-3 text-sm sm:grid-cols-3 md:grid-cols-4">
          <div><span className="block text-xs text-text-tertiary">Input tokens</span>{number(status.usage.input_tokens)}</div>
          <div><span className="block text-xs text-text-tertiary">Cached input</span>{number(status.usage.cached_input_tokens)}</div>
          <div><span className="block text-xs text-text-tertiary">Output tokens</span>{number(status.usage.output_tokens)}</div>
          <div><span className="block text-xs text-text-tertiary">Model calls</span>{number(status.usage.calls)}</div>
          {status.usage.cost_usd > 0 && <div><span className="block text-xs text-text-tertiary">Estimated cost</span>${status.usage.cost_usd.toFixed(2)}</div>}
          {results.filter(([, count]) => count > 0).map(([itemLabel, count]) => <div key={itemLabel}><span className="block text-xs text-text-tertiary">{itemLabel}</span>{number(count)}</div>)}
        </div>
      </div>
    );
  }

  return (
    <section className={`rounded-xl border border-border bg-surface-secondary p-4 shadow-sm ${className}`} aria-live="polite">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            {status.status === "failed" ? <ExclamationTriangleIcon className="h-5 w-5 text-danger" /> :
              !running ? <CheckCircleIcon className="h-5 w-5 text-success" /> :
                <span className="h-2.5 w-2.5 animate-pulse rounded-full bg-accent" />}
            <h2 className="text-sm font-semibold text-text-primary">Memory consolidation</h2>
          </div>
          <p className="mt-1 text-sm text-text-primary">
            {status.status === "retrying" ? "Waiting to retry" : stageName(status.stage)}
            {running && ` · Stage ${status.stageIndex} of ${status.stageCount}`}
          </p>
          <p className="mt-1 text-xs text-text-tertiary">
            {number(totalTokens)} tokens · {number(status.usage.calls)} model calls
            {status.usage.cost_usd > 0 && ` · $${status.usage.cost_usd.toFixed(2)}`}
            {` · Updated ${formatDistanceToNow(new Date(status.updatedAt), { addSuffix: true })}`}
          </p>
        </div>
      </div>

      <div className="mt-4 grid grid-cols-9 gap-1" aria-label={`Consolidation stage ${status.stageIndex} of ${status.stageCount}`}>
        {stages.map((label, index) => <div key={label} title={label} className={`h-1.5 rounded-full ${index + 1 < status.stageIndex || status.status === "completed" ? "bg-success" : index + 1 === status.stageIndex ? status.status === "failed" ? "bg-danger" : "bg-accent" : "bg-surface-tertiary"}`} />)}
      </div>

      {status.failure && (
        <p className={`mt-3 text-sm ${status.status === "failed" ? "text-danger" : "text-text-secondary"}`}>
          {status.failure.message}{status.failure.affectedCount > 0 && ` (${status.failure.affectedCount} affected items)`}
          {status.nextAttemptAt && ` · Retrying ${formatDistanceToNow(new Date(status.nextAttemptAt), { addSuffix: true })}`}
        </p>
      )}

      <details className="mt-3 border-t border-border pt-3 text-sm">
        <summary className="flex cursor-pointer list-none items-center gap-1 font-medium text-text-secondary hover:text-text-primary">
          <ChevronDownIcon className="h-4 w-4" /> Run details
        </summary>
        <div className="mt-3 grid grid-cols-2 gap-x-6 gap-y-2 sm:grid-cols-3">
          <div><span className="block text-xs text-text-tertiary">Input tokens</span>{number(status.usage.input_tokens)}</div>
          <div><span className="block text-xs text-text-tertiary">Cached input</span>{number(status.usage.cached_input_tokens)}</div>
          <div><span className="block text-xs text-text-tertiary">Output tokens</span>{number(status.usage.output_tokens)}</div>
          {results.filter(([, count]) => count > 0).map(([label, count]) => <div key={label}><span className="block text-xs text-text-tertiary">{label}</span>{number(count)}</div>)}
        </div>
      </details>
    </section>
  );
}
