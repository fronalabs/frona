"use client";

import dynamic from "next/dynamic";
import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AdjustmentsHorizontalIcon,
  MagnifyingGlassIcon,
  ShareIcon,
  XMarkIcon,
} from "@heroicons/react/24/outline";
import { getMemoryGraph, searchMemory } from "@/lib/api-client";
import { activeSearchMatches, branchColor } from "@/lib/memory-graph";
import type { MemoryGraphResponse, MemorySearchResult } from "@/lib/memory-types";
import { useMobile } from "@/lib/use-mobile";
import { MemoryInspector, type MemoryTab } from "./memory-inspector";
import { ConsolidationStatusCard } from "./consolidation-status-card";

const MemoryGraphCanvas = dynamic(
  () => import("./memory-graph-canvas").then((module) => module.MemoryGraphCanvas),
  { ssr: false },
);

function isTab(value: string | null): value is MemoryTab {
  return value === "page" || value === "structure" || value === "memory";
}

export function MemoryPage() {
  const pathname = usePathname();
  const searchParams = useSearchParams();
  const mobile = useMobile();
  const [graph, setGraph] = useState<MemoryGraphResponse | null>(null);
  const [graphError, setGraphError] = useState<string | null>(null);
  const [selectedPath, setSelectedPath] = useState(searchParams.get("page") ?? "");
  const [inspectorOpen, setInspectorOpen] = useState(true);
  const [searchMode, setSearchMode] = useState(!searchParams.get("page"));
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<MemorySearchResult[]>([]);
  const [searchLoading, setSearchLoading] = useState(false);
  const [showAsserted, setShowAsserted] = useState(true);
  const [showInferred, setShowInferred] = useState(true);
  const [filtersOpen, setFiltersOpen] = useState(false);
  const tab = isTab(searchParams.get("tab")) ? searchParams.get("tab") as MemoryTab : "page";
  const consolidationExpanded = searchParams.get("consolidation") === "expanded";

  useEffect(() => {
    const path = searchParams.get("page") ?? "";
    if (path) {
      setSelectedPath(path);
      setInspectorOpen(true);
    }
  }, [searchParams]);

  useEffect(() => {
    let cancelled = false;
    getMemoryGraph()
      .then((data) => {
        if (cancelled) return;
        setGraph(data);
        setSelectedPath((current) => current || data.selfPath || data.nodes[0]?.path || "");
      })
      .catch((reason) => {
        if (!cancelled) setGraphError(reason instanceof Error ? reason.message : "Memory is unavailable");
      });
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    const trimmed = query.trim();
    if (!trimmed) {
      setResults([]);
      setSearchLoading(false);
      return;
    }
    let cancelled = false;
    setSearchLoading(true);
    const timer = setTimeout(() => {
      searchMemory(trimmed)
        .then((items) => { if (!cancelled) setResults(items); })
        .catch(() => { if (!cancelled) setResults([]); })
        .finally(() => { if (!cancelled) setSearchLoading(false); });
    }, 250);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [query]);

  const writeSelection = useCallback((path: string, nextTab: MemoryTab = "page", replace = false) => {
    const params = new URLSearchParams(searchParams.toString());
    params.set("page", path);
    params.set("tab", nextTab);
    const destination = `${pathname}?${params.toString()}`;
    if (replace) window.history.replaceState(null, "", destination);
    else window.history.pushState(null, "", destination);
  }, [pathname, searchParams]);

  const selectPage = useCallback((path: string) => {
    setSelectedPath(path);
    setInspectorOpen(true);
    setSearchMode(false);
    writeSelection(path);
  }, [writeSelection]);

  const setActiveTab = useCallback((nextTab: MemoryTab) => {
    if (selectedPath) writeSelection(selectedPath, nextTab, true);
  }, [selectedPath, writeSelection]);

  const setConsolidationExpanded = useCallback((expanded: boolean) => {
    const params = new URLSearchParams(searchParams.toString());
    if (expanded) params.set("consolidation", "expanded");
    else params.delete("consolidation");
    const query = params.toString();
    window.history.replaceState(null, "", query ? `${pathname}?${query}` : pathname);
  }, [pathname, searchParams]);

  const openSearch = () => {
    setInspectorOpen(true);
    setSearchMode(true);
  };

  const searchMatches = useMemo(
    () => activeSearchMatches(searchMode, query, results),
    [query, results, searchMode],
  );
  const browsablePages = useMemo<MemorySearchResult[]>(
    () => graph?.nodes
      .map((node) => ({
        path: node.path,
        name: node.name,
        description: node.description,
        useCount: node.useCount,
        origin: node.origin,
        category: node.category,
        types: node.types.map((type) => type.iri),
        aliases: [],
      }))
      .sort((left, right) => {
        if (left.path === graph.selfPath) return -1;
        if (right.path === graph.selfPath) return 1;
        return right.useCount - left.useCount;
      }) ?? [],
    [graph],
  );

  if (graphError) {
    return (
      <main className="flex h-full items-center justify-center bg-surface p-6">
        <div className="max-w-md rounded-2xl border border-border bg-surface-secondary p-6 text-center shadow-sm">
          <ShareIcon className="mx-auto h-9 w-9 text-text-tertiary" />
          <h1 className="mt-3 text-xl font-semibold text-text-primary">Memory is unavailable</h1>
          <p className="mt-2 text-sm leading-5 text-text-secondary">The PKM memory backend is not active for this server. Enable it in server settings to browse the memory graph.</p>
          <Link href="/settings" className="mt-4 inline-flex rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover">Open settings</Link>
          <p className="mt-3 text-xs text-text-tertiary">{graphError}</p>
        </div>
      </main>
    );
  }

  if (!graph) {
    return <main className="flex h-full items-center justify-center bg-surface text-sm text-text-secondary">Loading memory graph…</main>;
  }

  if (graph.nodes.length === 0) {
    return (
      <main className="flex h-full items-center justify-center bg-surface p-6">
        <div className="max-w-md rounded-2xl border border-border bg-surface-secondary p-6 text-center shadow-sm">
          <ShareIcon className="mx-auto h-9 w-9 text-text-tertiary" />
          <h1 className="mt-3 text-xl font-semibold text-text-primary">No memories yet</h1>
          <p className="mt-2 text-sm leading-5 text-text-secondary">
            Your memory graph will appear here after Frona learns from your conversations.
          </p>
          <Link href="/" className="mt-4 inline-flex rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover">
            Start a conversation
          </Link>
        </div>
      </main>
    );
  }

  if (!selectedPath) {
    return <main className="flex h-full items-center justify-center bg-surface text-sm text-text-secondary">Loading memory graph…</main>;
  }

  return (
    <main className="relative h-full overflow-hidden bg-surface">
      <MemoryGraphCanvas
        data={graph}
        selectedPath={selectedPath}
        searchMatches={searchMatches}
        showAsserted={showAsserted}
        showInferred={showInferred}
        onSelect={selectPage}
      />

      <div className="absolute left-3 right-3 top-3 z-20 flex items-start gap-2 sm:left-5 sm:right-auto sm:w-[min(780px,calc(100%-40px))]">
        <div className="flex min-w-0 flex-1 items-center rounded-xl border border-border bg-surface-secondary/95 shadow-lg backdrop-blur">
          <MagnifyingGlassIcon className="ml-3 h-5 w-5 shrink-0 text-text-tertiary" />
          <input
            value={query}
            onChange={(event) => { setQuery(event.target.value); openSearch(); }}
            onFocus={openSearch}
            placeholder={`Search ${graph.nodes.length} memory pages`}
            className="min-w-0 flex-1 bg-transparent px-3 py-3 text-sm text-text-primary outline-none placeholder:text-text-tertiary"
          />
          {query && (
            <button onClick={() => setQuery("")} className="mr-1 rounded-lg p-2 text-text-tertiary hover:bg-surface-tertiary hover:text-text-primary" aria-label="Clear search">
              <XMarkIcon className="h-4 w-4" />
            </button>
          )}
        </div>
        <button
          onClick={() => setFiltersOpen((open) => !open)}
          className={`rounded-xl border border-border bg-surface-secondary/95 p-3 shadow-lg backdrop-blur hover:text-text-primary ${filtersOpen ? "text-text-primary" : "text-text-secondary"}`}
          title="Graph legend and filters"
        >
          <AdjustmentsHorizontalIcon className="h-5 w-5" />
        </button>
        <ConsolidationStatusCard
          compact
          expanded={consolidationExpanded}
          onExpandedChange={setConsolidationExpanded}
          className="relative shrink-0"
        />
      </div>

      {filtersOpen && (
        <div className="absolute left-3 right-3 top-[68px] z-20 rounded-xl border border-border bg-surface-secondary/95 p-4 shadow-xl backdrop-blur sm:left-[calc(min(520px,calc(100%-40px))+28px)] sm:right-auto sm:w-64 sm:-translate-x-full">
          <div className="flex items-center justify-between">
            <h2 className="text-sm font-semibold text-text-primary">Graph display</h2>
            <button onClick={() => setFiltersOpen(false)} className="text-text-tertiary hover:text-text-primary"><XMarkIcon className="h-4 w-4" /></button>
          </div>
          <div className="mt-3 space-y-2 border-b border-border pb-3 text-sm text-text-secondary">
            <label className="flex cursor-pointer items-center justify-between gap-3">
              <span>Asserted relations</span>
              <input type="checkbox" checked={showAsserted} onChange={(event) => setShowAsserted(event.target.checked)} className="accent-accent" />
            </label>
            <label className="flex cursor-pointer items-center justify-between gap-3">
              <span>Inferred relations</span>
              <input type="checkbox" checked={showInferred} onChange={(event) => setShowInferred(event.target.checked)} className="accent-accent" />
            </label>
          </div>
          <p className="mt-3 text-[11px] font-semibold uppercase tracking-wider text-text-tertiary">Ontology branches</p>
          <div className="mt-2 max-h-52 space-y-2 overflow-y-auto">
            {graph.legend.map((branch) => (
              <div key={branch.iri} className="flex items-center gap-2 text-xs text-text-secondary">
                <span className="h-3 w-3 shrink-0 rounded-full border-[3px] border-current bg-surface" style={{ color: branchColor(branch.iri) }} />
                <span className="truncate" title={branch.iri}>{branch.label}</span>
              </div>
            ))}
          </div>
          <p className="mt-3 text-[11px] leading-4 text-text-tertiary">Solid arrows are asserted. Lighter lines are inferred. Dashed lines connect pages constructed from shared memory. Select a node to emphasize two levels of related memory.</p>
        </div>
      )}

      <div className="pointer-events-none absolute bottom-4 right-4 z-10 rounded-lg bg-surface-secondary/80 px-2.5 py-1.5 text-[11px] text-text-tertiary backdrop-blur">
        {graph.nodes.length} pages · {graph.edges.length} relations
      </div>

      <MemoryInspector
        open={inspectorOpen}
        mobile={mobile}
        selectedPath={selectedPath}
        tab={tab}
        searchMode={searchMode}
        searchQuery={query}
        searchResults={results}
        browsablePages={browsablePages}
        searchLoading={searchLoading}
        onClose={() => setInspectorOpen(false)}
        onSelect={selectPage}
        onTab={setActiveTab}
        onBackToSearch={() => { setInspectorOpen(true); setSearchMode(true); }}
      />
    </main>
  );
}
