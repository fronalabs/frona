"use client";

import { BellAlertIcon, BellSlashIcon } from "@heroicons/react/24/outline";
import { ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

export function humanizeInterval(minutes: number): string {
  if (minutes === 0) return "Disabled";
  if (minutes === 1) return "Every minute";
  if (minutes < 60) return `Every ${minutes} minutes`;
  if (minutes === 60) return "Every hour";
  if (minutes % 60 === 0) {
    const h = minutes / 60;
    return `Every ${h} hours`;
  }
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  return `Every ${h}h ${m}m`;
}

function formatTime(iso: string, timezone?: string): string {
  try {
    const d = new Date(iso);
    return new Intl.DateTimeFormat("en-US", {
      year: "numeric",
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      timeZone: timezone,
      timeZoneName: "short",
    }).format(d);
  } catch {
    return iso;
  }
}

interface ParsedResult {
  interval?: number | null;
  nextAt?: string | null;
}

function parseResult(result: unknown): ParsedResult | null {
  let obj: unknown = null;
  if (typeof result === "string") {
    try {
      obj = JSON.parse(result);
    } catch {
      return null;
    }
  } else if (result && typeof result === "object") {
    obj = result;
  }
  if (!obj || typeof obj !== "object") return null;
  const o = obj as Record<string, unknown>;
  if (!("heartbeat_interval" in o) && !("next_heartbeat_at" in o)) {
    return null;
  }
  if (
    o.heartbeat_interval !== undefined &&
    o.heartbeat_interval !== null &&
    typeof o.heartbeat_interval !== "number"
  ) {
    return null;
  }
  if (
    o.next_heartbeat_at !== undefined &&
    o.next_heartbeat_at !== null &&
    typeof o.next_heartbeat_at !== "string"
  ) {
    return null;
  }
  return {
    interval:
      typeof o.heartbeat_interval === "number"
        ? o.heartbeat_interval
        : o.heartbeat_interval === null
          ? null
          : undefined,
    nextAt:
      typeof o.next_heartbeat_at === "string"
        ? o.next_heartbeat_at
        : o.next_heartbeat_at === null
          ? null
          : undefined,
  };
}

export const HeartbeatView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const argsInterval =
    typeof a.interval_minutes === "number" ? a.interval_minutes : null;

  const parsed = parseResult(result);
  if (result !== undefined && parsed === null) {
    return <ToolViewFallback />;
  }

  const interval = parsed?.interval ?? argsInterval;
  const nextAt = parsed?.nextAt ?? null;

  const enabled = typeof interval === "number" && interval > 0;
  const subtitle =
    typeof interval === "number" ? humanizeInterval(interval) : null;

  const expandable = typeof interval === "number";

  return (
    <ToolRow status={status} expandable={expandable}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Heartbeat</ToolRow.Title>
        <ToolRow.Subtitle>{subtitle}</ToolRow.Subtitle>
      </ToolRow.Header>
      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex flex-col gap-2">
          <div className="rounded-lg bg-surface-nav p-4 flex flex-col gap-2 text-[0.8125rem]">
            <div className="flex items-center gap-2">
              {enabled ? (
                <BellAlertIcon className="h-4 w-4 shrink-0 text-accent" />
              ) : (
                <BellSlashIcon className="h-4 w-4 shrink-0 text-text-tertiary" />
              )}
              <span className="font-medium text-text-primary">
                {typeof interval === "number"
                  ? humanizeInterval(interval)
                  : "-"}
              </span>
            </div>
            {enabled && nextAt && (
              <div className="text-xs text-text-tertiary pl-6">
                Next heartbeat:{" "}
                <span className="text-text-secondary">{formatTime(nextAt)}</span>
              </div>
            )}
          </div>
        </div>
      </ToolRow.Body>
    </ToolRow>
  );
};
