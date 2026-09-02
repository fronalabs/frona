"use client";

import { ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

interface SearchHit {
  index: number;
  title: string;
  url: string;
  snippet: string;
}

/**
 * Parse the backend-formatted search result text:
 *
 *   1. Title
 *      url
 *      snippet (one or more lines)
 *
 *   2. Title
 *      url
 *      snippet
 *
 * Returns null on parse failure so caller can fall back to raw text.
 */
function parseSearchResult(text: string): SearchHit[] | null {
  if (!text || text.trim() === "No results found.") return [];
  const blocks = text.split(/\n\n+/);
  const hits: SearchHit[] = [];
  for (const block of blocks) {
    const lines = block.split("\n");
    if (lines.length < 3) return null;
    const head = lines[0].match(/^(\d+)\.\s+(.*)$/);
    if (!head) return null;
    const url = lines[1].trim();
    const snippet = lines
      .slice(2)
      .map((l) => l.replace(/^\s+/, ""))
      .join(" ")
      .trim();
    hits.push({
      index: Number(head[1]),
      title: head[2].trim(),
      url,
      snippet,
    });
  }
  return hits;
}

export const WebSearchView: ToolView = ({
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
  const hits = parseSearchResult(resultText);
  if (result !== undefined && hits === null) {
    return <ToolViewFallback />;
  }

  return (
    <ToolRow status={status} expandable={resultText.length > 0}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Web Search</ToolRow.Title>
        <ToolRow.Subtitle>{query || null}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="p-3">
          {!hits || hits.length === 0 ? (
            <p className="text-xs text-text-tertiary m-0">No results found.</p>
          ) : (
            <ol className="flex flex-col gap-3 m-0 p-0 list-none">
              {hits.map((hit) => (
                <li key={`${hit.index}-${hit.url}`} className="flex flex-col gap-0.5">
                  <a
                    href={hit.url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-sm font-medium text-accent hover:underline break-words"
                  >
                    {hit.title}
                  </a>
                  <span className="text-xs font-mono text-text-tertiary break-all">
                    {hit.url}
                  </span>
                  {hit.snippet && (
                    <p className="text-xs text-text-secondary m-0">{hit.snippet}</p>
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
