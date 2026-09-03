"use client";

import { DocumentTextIcon } from "@heroicons/react/24/outline";
import { shouldUseToolViewFallback, ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

interface MemoryHit {
  name: string;
  tag: string;
  description: string;
  path: string;
  snippets: string[];
}

interface ParsedMemoryResult {
  hits: MemoryHit[];
  truncated: boolean;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function toMemoryVaultRelativePath(file: string): string {
  const normalized = file.replaceAll("\\", "/");
  const memoryRoot = "/Memory/";
  const rootIndex = normalized.lastIndexOf(memoryRoot);

  if (rootIndex >= 0) {
    return normalized.slice(rootIndex + memoryRoot.length);
  }

  return normalized.startsWith("Memory/")
    ? normalized.slice("Memory/".length)
    : normalized;
}

function parseStructuredMemoryResult(value: unknown): ParsedMemoryResult | null {
  if (!isRecord(value) || !Array.isArray(value.results)) return null;

  const hits: MemoryHit[] = [];
  for (const result of value.results) {
    if (!isRecord(result)) return null;
    if (
      typeof result.name !== "string" ||
      typeof result.file !== "string" ||
      typeof result.description !== "string" ||
      typeof result.category !== "string" ||
      !Array.isArray(result.matched_by)
    ) {
      return null;
    }

    const snippets = result.matched_by.flatMap((match) => {
      if (
        isRecord(match) &&
        match.kind === "body_text" &&
        typeof match.snippet === "string"
      ) {
        return [match.snippet];
      }
      return [];
    });

    hits.push({
      name: result.name,
      tag: result.category,
      description: result.description,
      path: toMemoryVaultRelativePath(result.file),
      snippets,
    });
  }

  return { hits, truncated: value.truncated === true };
}

function parseMemoryResult(text: string): ParsedMemoryResult | null {
  const trimmed = text.trim();
  if (!trimmed) return { hits: [], truncated: false };
  if (trimmed.startsWith("No pages matched")) return { hits: [], truncated: false };

  try {
    const structured = parseStructuredMemoryResult(JSON.parse(trimmed));
    if (structured) return structured;
  } catch {}

  const hits: MemoryHit[] = [];
  for (const block of text.split(/\n\n+/)) {
    const lines = block.split("\n").filter((l) => l.trim().length > 0);
    if (lines.length === 0) continue;
    if (!lines[0].startsWith("- ")) continue;

    const head = lines[0].match(/^-\s+(.*?)\s+\[(.+?)\]\s*$/);
    if (!head || lines.length < 3) return null;
    hits.push({
      name: head[1].trim(),
      tag: head[2].trim(),
      path: toMemoryVaultRelativePath(lines[lines.length - 1].trim()),
      description: lines
        .slice(1, -1)
        .map((l) => l.trim())
        .join(" ")
        .trim(),
      snippets: [],
    });
  }

  // Non-empty text that yielded no items is a format mismatch, not "no results".
  return hits.length > 0 ? { hits, truncated: false } : null;
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
  const parsed = parseMemoryResult(resultText);
  if (shouldUseToolViewFallback(result, parsed !== null, status)) {
    return <ToolViewFallback />;
  }

  const hits = parsed?.hits;
  const subtitle =
    hits && hits.length > 0
      ? `${query ? `${query} - ` : ""}${hits.length}${parsed.truncated ? "+" : ""} ${hits.length === 1 && !parsed.truncated ? "match" : "matches"}`
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
                  {hit.snippets.map((snippet) => (
                    <p key={snippet} className="text-xs text-text-secondary m-0">
                      <span className="text-text-tertiary">Match: </span>
                      <span>{snippet}</span>
                    </p>
                  ))}
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
