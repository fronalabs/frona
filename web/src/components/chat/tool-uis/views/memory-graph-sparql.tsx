"use client";

import { useEffect, useState } from "react";
import { CheckCircleIcon, XCircleIcon } from "@heroicons/react/24/outline";
import { CodeBlock } from "@/components/ui/code-block";
import { shouldUseToolViewFallback, ToolViewFallback } from "./safe-tool-view";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

interface BooleanResult {
  kind: "boolean";
  value: boolean;
}

interface SolutionsResult {
  kind: "solutions";
  columns: string[];
  rows: Record<string, string>[];
  truncated: boolean;
}

type SparqlResult = BooleanResult | SolutionsResult;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseSparqlResult(value: unknown): SparqlResult | null {
  if (!isRecord(value)) return null;
  if (value.kind === "boolean" && typeof value.value === "boolean") {
    return { kind: "boolean", value: value.value };
  }
  if (
    value.kind !== "solutions" ||
    !Array.isArray(value.columns) ||
    !value.columns.every((column) => typeof column === "string") ||
    !Array.isArray(value.rows)
  ) {
    return null;
  }

  const rows: Record<string, string>[] = [];
  for (const row of value.rows) {
    if (!isRecord(row)) return null;
    const parsedRow: Record<string, string> = {};
    for (const [column, cell] of Object.entries(row)) {
      if (typeof cell !== "string") return null;
      parsedRow[column] = cell;
    }
    rows.push(parsedRow);
  }

  return {
    kind: "solutions",
    columns: value.columns,
    rows,
    truncated: value.truncated === true,
  };
}

function decodeResult(result: unknown): {
  parsed: SparqlResult | null;
  raw: string;
} {
  if (result === undefined) return { parsed: null, raw: "" };
  if (typeof result !== "string") {
    return {
      parsed: parseSparqlResult(result),
      raw: JSON.stringify(result, null, 2),
    };
  }
  try {
    return { parsed: parseSparqlResult(JSON.parse(result)), raw: result };
  } catch {
    return { parsed: null, raw: result };
  }
}

function resultSubtitle(result: SparqlResult | null): string | null {
  if (!result) return null;
  if (result.kind === "boolean") return `ASK ${result.value}`;
  const count = `${result.rows.length}${result.truncated ? "+" : ""}`;
  return `${count} ${result.rows.length === 1 && !result.truncated ? "row" : "rows"}`;
}

function SolutionsTable({ result }: { result: SolutionsResult }) {
  if (result.rows.length === 0) {
    return <p className="m-0 px-3 pb-3 text-xs text-text-tertiary">No rows.</p>;
  }

  return (
    <div className="max-h-80 overflow-auto border-t border-border">
      <table className="min-w-full border-separate border-spacing-0 text-left text-xs">
        <thead className="sticky top-0 z-10 bg-surface-tertiary text-text-tertiary">
          <tr>
            {result.columns.map((column) => (
              <th
                key={column}
                scope="col"
                className="border-b border-border px-3 py-2 font-mono font-medium whitespace-nowrap"
              >
                ?{column}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {result.rows.map((row, rowIndex) => (
            <tr key={`${rowIndex}-${JSON.stringify(row)}`} className="even:bg-surface-tertiary/30">
              {result.columns.map((column) => (
                <td
                  key={column}
                  className="border-b border-border px-3 py-2 font-mono text-text-secondary whitespace-nowrap last:border-r-0"
                >
                  {row[column] ?? (
                    <span className="font-sans italic text-text-tertiary">unbound</span>
                  )}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {result.truncated && (
        <p className="m-0 px-3 py-2 text-xs text-text-tertiary">More rows not shown.</p>
      )}
    </div>
  );
}

function BooleanValue({ value }: { value: boolean }) {
  const Icon = value ? CheckCircleIcon : XCircleIcon;
  return (
    <div className="px-3 pb-3">
      <span
        className={`inline-flex items-center gap-1.5 rounded-full px-2 py-1 text-xs font-medium ${
          value ? "bg-success-bg text-success-text" : "bg-danger-bg text-danger-text"
        }`}
      >
        <Icon className="h-4 w-4" />
        {String(value)}
      </span>
    </div>
  );
}

function FormattedQuery({ query }: { query: string }) {
  const [formatted, setFormatted] = useState(query);

  useEffect(() => {
    let active = true;
    setFormatted(query);
    void import("sparql-formatter")
      .then(({ spfmt }) => {
        const next = spfmt.format(query);
        if (active && next.trim()) setFormatted(next.trim());
      })
      .catch(() => undefined);
    return () => {
      active = false;
    };
  }, [query]);

  return <CodeBlock code={formatted} language="sparql" wrap />;
}

export const MemoryGraphSparqlView: ToolView = ({
  args,
  result,
  status,
  isExpanded,
  onToggle,
}) => {
  const values = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
  const query = typeof values.query === "string" ? values.query : "";
  const { parsed, raw } = decodeResult(result);
  if (shouldUseToolViewFallback(result, parsed !== null, status)) {
    return <ToolViewFallback />;
  }

  const error =
    status?.type === "incomplete"
      ? (status as { error?: unknown }).error
      : undefined;
  const errorText =
    error == null ? null : typeof error === "string" ? error : JSON.stringify(error);
  const expandable = query.length > 0 || raw.length > 0;

  return (
    <ToolRow status={status} expandable={expandable}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Memory Graph Query</ToolRow.Title>
        <ToolRow.Subtitle>{resultSubtitle(parsed)}</ToolRow.Subtitle>
      </ToolRow.Header>

      <ToolRow.Body isExpanded={isExpanded} unstyled>
        <div className="flex flex-col gap-3">
          {query && <FormattedQuery query={query} />}
          {parsed?.kind === "solutions" ? (
            <SolutionsTable result={parsed} />
          ) : parsed?.kind === "boolean" ? (
            <BooleanValue value={parsed.value} />
          ) : null}
        </div>
      </ToolRow.Body>

      <ToolRow.Error>
        <div className="flex flex-col gap-3">
          {query && <FormattedQuery query={query} />}
          <div className="px-3 pb-3 text-xs">
            <p className="font-semibold text-danger">SPARQL query failed</p>
            {errorText && (
              <pre className="mt-1 whitespace-pre-wrap text-text-tertiary">{errorText}</pre>
            )}
          </div>
        </div>
      </ToolRow.Error>
    </ToolRow>
  );
};
