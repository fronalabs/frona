"use client";

import { DocumentTextIcon } from "@heroicons/react/24/outline";
import { ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

interface MemoryHit {
  name: string;
  tag: string;
  description: string;
  path: string;
}

/**
 * Parse the `memory_search` result text emitted by the backend
 * (crates/frona-server/src/memory/pkm/tools.rs). Keep in lockstep:
 *
 *   Top matches - read(<memory-root>/<path>.md) to open one:
 *
 *   - Name  [tag]
 *     description
 *     path/to/page
 *
 *   - Name  [tag]
 *     ...
 *
 * Returns [] for the empty ("No pages matched") case and null on parse
 * failure so the caller can fall back to raw text.
 */
function parseMemoryResult(text: string): MemoryHit[] | null {
  const trimmed = text.trim();
  if (!trimmed) return [];
  if (trimmed.startsWith("No pages matched")) return [];

  const hits: MemoryHit[] = [];
  for (const block of text.split(/\n\n+/)) {
    const lines = block.split("\n").filter((l) => l.trim().length > 0);
    if (lines.length === 0) continue;
    // The header line ("Top matches - …") and any stray prose don't start an item.
    if (!lines[0].startsWith("- ")) continue;

    const head = lines[0].match(/^-\s+(.*?)\s+\[(.+?)\]\s*$/);
    if (!head || lines.length < 3) return null;
    hits.push({
      name: head[1].trim(),
      tag: head[2].trim(),
      path: lines[lines.length - 1].trim(),
      description: lines
        .slice(1, -1)
        .map((l) => l.trim())
        .join(" ")
        .trim(),
    });
  }

  // Non-empty text that yielded no items is a format mismatch, not "no results".
  return hits.length > 0 ? hits : null;
}

export const MemorySearchView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const query = typeof a.query === "string" ? a.query : "";
  const resultText =
    typeof result === "string"
      ? result
      : result !== undefined
        ? JSON.stringify(result, null, 2)
        : "";
  const hits = parseMemoryResult(resultText);
  if (result !== undefined && hits === null) {
    return <ToolViewFallback />;
  }

  const subtitle =
    hits && hits.length > 0
      ? `${query ? `${query} · ` : ""}${hits.length} ${hits.length === 1 ? "match" : "matches"}`
      : query || null;

  return (
    <ToolRow status={status} expandable={resultText.length > 0}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Memory Search</ToolRow.Title>
        <ToolRow.Subtitle>{subtitle}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="p-3">
          {!hits || hits.length === 0 ? (
            <p className="text-xs text-text-tertiary m-0">No pages matched.</p>
          ) : (
            <ol className="flex flex-col gap-3 m-0 p-0 list-none">
              {hits.map((hit, i) => (
                <li key={`${i}-${hit.path}`} className="flex flex-col gap-0.5">
                  <div className="flex items-center gap-1.5">
                    <span className="text-sm font-medium text-text-primary break-words">
                      {hit.name}
                    </span>
                    {hit.tag && (
                      <span className="inline-flex items-center rounded px-1.5 py-0.5 text-[10px] font-medium bg-surface-tertiary text-text-tertiary">
                        {hit.tag}
                      </span>
                    )}
                  </div>
                  {hit.description && (
                    <p className="text-xs text-text-secondary m-0">{hit.description}</p>
                  )}
                  {hit.path && (
                    <span className="inline-flex items-center gap-1 text-xs font-mono text-text-tertiary break-all">
                      <DocumentTextIcon className="h-3 w-3 shrink-0" />
                      {hit.path}
                    </span>
                  )}
                </li>
              ))}
            </ol>
          )}
        </div>
      </ToolRow.Body>
    </ToolRow>
  );
};
