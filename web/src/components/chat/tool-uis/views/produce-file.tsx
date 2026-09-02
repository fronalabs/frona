"use client";

import { DocumentIcon } from "@heroicons/react/24/outline";
import { ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

export const ProduceFileView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const path = typeof a.path === "string" ? a.path : null;
  const description =
    typeof a.description === "string" && a.description !== "produce_file"
      ? a.description
      : null;
  const subtitle = description ?? path;

  let parsed: Record<string, unknown> | null = null;
  if (typeof result === "string") {
    try {
      const v = JSON.parse(result);
      if (v && typeof v === "object") parsed = v as Record<string, unknown>;
    } catch {
      // not JSON; fall through
    }
  } else if (result && typeof result === "object") {
    parsed = result as Record<string, unknown>;
  }

  const resultMatchesView =
    parsed !== null &&
    (typeof parsed.filename === "string" ||
      typeof parsed.content_type === "string" ||
      typeof parsed.size_bytes === "number");
  if (result !== undefined && !resultMatchesView) {
    return <ToolViewFallback />;
  }

  const filename =
    parsed && typeof parsed.filename === "string" ? parsed.filename : path;
  const contentType =
    parsed && typeof parsed.content_type === "string" ? parsed.content_type : null;
  const sizeBytes =
    parsed && typeof parsed.size_bytes === "number" ? parsed.size_bytes : null;

  const hasContent = filename != null || contentType != null || sizeBytes != null;

  return (
    <ToolRow status={status} expandable={hasContent}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Produce File</ToolRow.Title>
        <ToolRow.Subtitle>{subtitle}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex items-center gap-3 p-3 text-xs">
          <DocumentIcon className="h-8 w-8 shrink-0 text-text-tertiary" />
          <div className="flex flex-col gap-0.5 min-w-0">
            {filename && (
              <div className="font-mono text-text-primary truncate">{filename}</div>
            )}
            {(contentType || sizeBytes !== null) && (
              <div className="flex gap-3 text-text-tertiary">
                {sizeBytes !== null && <span>{formatBytes(sizeBytes)}</span>}
                {contentType && <span>{contentType}</span>}
              </div>
            )}
          </div>
        </div>
      </ToolRow.Body>
    </ToolRow>
  );
};
