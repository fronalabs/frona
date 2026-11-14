"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { MultiDirectedGraph } from "graphology";
import forceAtlas2 from "graphology-layout-forceatlas2";
import FA2LayoutSupervisor from "graphology-layout-forceatlas2/worker";
import Sigma, { DEFAULT_STYLES } from "sigma";
import { extremityArrow, pathCurved, pathLine, pathLoop } from "sigma/rendering";
import type { NodeReducer, EdgeReducer } from "sigma/types";
import { ArrowsPointingOutIcon, HomeIcon, MinusIcon, PlusIcon } from "@heroicons/react/24/outline";
import { branchColor, canvasResolution, neighborhoodByDepth, seededPosition, selectionCameraTarget, wrapLabel } from "@/lib/memory-graph";
import type { MemoryGraphNode, MemoryGraphResponse } from "@/lib/memory-types";

type HoverCard = { node: MemoryGraphNode; x: number; y: number } | null;

interface MemoryGraphCanvasProps {
  data: MemoryGraphResponse;
  selectedPath: string;
  searchMatches: Set<string> | null;
  showAsserted: boolean;
  showInferred: boolean;
  onSelect: (path: string) => void;
}

interface NodeAttributes {
  x: number;
  y: number;
  size: number;
  color: string;
  baseColor: string;
  label: string;
}

interface EdgeAttributes {
  size: number;
  color: string;
  label: string;
  origin: "asserted" | "inferred" | "memory";
}

const fadedNode = "rgba(128, 137, 148, 0.16)";
const ambientAssertedEdge = "rgba(105, 116, 128, 0.24)";
const ambientInferredEdge = "rgba(105, 116, 128, 0.14)";
const layoutCacheVersion = 2;

function colorForNode(node: MemoryGraphNode): string {
  const selectedType = node.types.find((type) => type.iri === node.displayType);
  return branchColor(node.colorBranch, selectedType?.ancestors.length ?? 0);
}

function loadPositions(revision: string): Record<string, { x: number; y: number }> | null {
  try {
    const raw = localStorage.getItem(`frona:memory-layout:v${layoutCacheVersion}:${revision}`);
    return raw ? JSON.parse(raw) : null;
  } catch {
    return null;
  }
}

function savePositions(revision: string, graph: MultiDirectedGraph<NodeAttributes, EdgeAttributes>) {
  const positions: Record<string, { x: number; y: number }> = {};
  graph.forEachNode((path, attributes) => {
    positions[path] = { x: attributes.x, y: attributes.y };
  });
  try {
    localStorage.setItem(`frona:memory-layout:v${layoutCacheVersion}:${revision}`, JSON.stringify(positions));
  } catch {
    // Layout caching is an enhancement; private browsing and quotas may disable it.
  }
}

export function MemoryGraphCanvas({
  data,
  selectedPath,
  searchMatches,
  showAsserted,
  showInferred,
  onSelect,
}: MemoryGraphCanvasProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<Sigma<NodeAttributes, EdgeAttributes> | null>(null);
  const nodeReducerRef = useRef<NodeReducer<NodeAttributes, EdgeAttributes>>((_key, display) => display);
  const edgeReducerRef = useRef<EdgeReducer<NodeAttributes, EdgeAttributes>>((_key, display) => display);
  const graphRef = useRef<MultiDirectedGraph<NodeAttributes, EdgeAttributes> | null>(null);
  const onSelectRef = useRef(onSelect);
  const selectedPathRef = useRef(selectedPath);
  const [hover, setHover] = useState<HoverCard>(null);
  const nodeByPath = useMemo(() => new Map(data.nodes.map((node) => [node.path, node])), [data.nodes]);
  const depths = useMemo(
    () => neighborhoodByDepth(selectedPath, data.edges, 2),
    [data.edges, selectedPath],
  );
  const directlyRelated = useMemo(() => {
    const paths = new Set<string>();
    for (const edge of data.edges) {
      if (edge.origin === "memory") continue;
      if (edge.fromPath === selectedPath) paths.add(edge.toPath);
      if (edge.toPath === selectedPath) paths.add(edge.fromPath);
    }
    return paths;
  }, [data.edges, selectedPath]);

  useEffect(() => {
    onSelectRef.current = onSelect;
  }, [onSelect]);

  useEffect(() => {
    selectedPathRef.current = selectedPath;
  }, [selectedPath]);

  useEffect(() => {
    if (!containerRef.current) return;

    const graph = new MultiDirectedGraph<NodeAttributes, EdgeAttributes>();
    const theme = getComputedStyle(document.documentElement);
    let labelColor = theme.getPropertyValue("--text-secondary").trim() || "#6b7280";
    let edgeLabelColor = theme.getPropertyValue("--text-tertiary").trim() || "#9ca3af";
    const cached = loadPositions(data.revision);
    const branches = [...new Set(data.nodes.map((node) => node.colorBranch))].sort();
    const branchIndex = new Map(branches.map((branch, index) => [branch, index]));

    for (const node of data.nodes) {
      const fallback = seededPosition(node.path);
      const index = branchIndex.get(node.colorBranch) ?? 0;
      const angle = branches.length > 1 ? (index / branches.length) * Math.PI * 2 : 0;
      const position = cached?.[node.path] ?? {
        x: Math.cos(angle) * 1.4 + fallback.x,
        y: Math.sin(angle) * 1.4 + fallback.y,
      };
      const color = colorForNode(node);
      graph.addNode(node.path, {
        ...position,
        size: node.path === data.selfPath ? 8 : 5,
        color,
        baseColor: color,
        label: node.name,
      });
    }

    for (const edge of data.edges) {
      if (!graph.hasNode(edge.fromPath) || !graph.hasNode(edge.toPath)) continue;
      const memoryEdge = edge.origin === "memory";
      graph.addDirectedEdgeWithKey(edge.id, edge.fromPath, edge.toPath, {
        size: edge.origin === "asserted" ? 0.7 : 0.4,
        color: memoryEdge ? "rgba(0, 0, 0, 0)" : edge.origin === "asserted" ? "rgba(105, 116, 128, 0.44)" : "rgba(105, 116, 128, 0.25)",
        label: edge.label,
        origin: edge.origin,
      });
    }

    const renderer = new Sigma<NodeAttributes, EdgeAttributes>(graph, containerRef.current, {
      primitives: {
        nodes: { label: { font: { size: 14, weight: "500" }, margin: 3 } },
        edges: { paths: [pathLine(), pathCurved(), pathLoop()], extremities: [extremityArrow()] },
      },
      styles: {
        nodes: {
          ...DEFAULT_STYLES.nodes,
          labelColor: "transparent",
          labelSize: 14,
          labelFont: "Arial",
          backdropVisibility: "hidden",
        },
        edges: {
          ...DEFAULT_STYLES.edges,
          parallelPath: "curved",
          head: (attributes) => attributes.origin === "asserted" ? "arrow" : "none",
        },
      },
      settings: {
        allowInvalidContainer: true,
        itemSizesReference: "screen",
        labelDensity: 0.18,
        labelGridCellSize: 110,
        labelRenderedSizeThreshold: 4,
        minEdgeThickness: 0.35,
        renderEdgeLabels: true,
      },
      nodeReducer: (...args) => ({
        ...nodeReducerRef.current(...args),
        ...(args[3].isHovered ? { labelVisibility: "visible" as const } : {}),
      }),
      edgeReducer: (...args) => ({ ...edgeReducerRef.current(...args), labelColor: edgeLabelColor }),
    });

    const themeObserver = new MutationObserver(() => {
      const currentTheme = getComputedStyle(document.documentElement);
      labelColor = currentTheme.getPropertyValue("--text-secondary").trim() || "#6b7280";
      edgeLabelColor = currentTheme.getPropertyValue("--text-tertiary").trim() || "#9ca3af";
      renderer.refresh();
    });
    themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

    const memoryEdges = data.edges.filter((edge) => edge.origin === "memory");
    const memoryCanvas = renderer.createCanvas("memoryEdges", {
      beforeLayer: "stage",
      style: { pointerEvents: "none" },
    });
    const memoryContext = memoryCanvas.getContext("2d");
    const drawMemoryEdges = () => {
      if (!memoryContext) return;
      const width = renderer.getContainer().clientWidth;
      const height = renderer.getContainer().clientHeight;
      const ratio = window.devicePixelRatio || 1;
      const resolution = canvasResolution(width, height, ratio);
      memoryCanvas.style.width = resolution.cssWidth;
      memoryCanvas.style.height = resolution.cssHeight;
      if (memoryCanvas.width !== resolution.pixelWidth || memoryCanvas.height !== resolution.pixelHeight) {
        memoryCanvas.width = resolution.pixelWidth;
        memoryCanvas.height = resolution.pixelHeight;
        memoryContext.setTransform(ratio, 0, 0, ratio, 0, 0);
      }
      memoryContext.clearRect(0, 0, width, height);
      const currentTheme = getComputedStyle(document.documentElement);
      const selectedColor = currentTheme.getPropertyValue("--text-secondary").trim() || "#6b7280";
      const visibleMemoryEdges = memoryEdges.filter((edge) => (
        edge.fromPath === selectedPathRef.current || edge.toPath === selectedPathRef.current
      ));
      const denseSelection = visibleMemoryEdges.length > 24;
      for (const edge of visibleMemoryEdges) {
        const sourceData = renderer.getNodeDisplayData(edge.fromPath);
        const targetData = renderer.getNodeDisplayData(edge.toPath);
        if (!sourceData || !targetData) continue;
        const source = renderer.framedGraphToViewport(sourceData);
        const target = renderer.framedGraphToViewport(targetData);
        memoryContext.beginPath();
        memoryContext.setLineDash([5, 7]);
        memoryContext.lineWidth = denseSelection ? 0.55 : 0.7;
        memoryContext.globalAlpha = denseSelection ? 0.24 : 0.38;
        memoryContext.strokeStyle = selectedColor;
        memoryContext.moveTo(source.x, source.y);
        memoryContext.lineTo(target.x, target.y);
        memoryContext.stroke();
      }
      memoryContext.globalAlpha = 1;
      memoryContext.setLineDash([]);
    };
    // Sigma selects visible labels; a canvas overlay preserves our multiline labels.
    const labelCanvas = renderer.createCanvas("wrappedLabels", {
      afterLayer: "stage",
      style: { pointerEvents: "none" },
    });
    const labelContext = labelCanvas.getContext("2d");
    const drawLabels = () => {
      if (!labelContext) return;
      const { width, height } = renderer.getDimensions();
      const ratio = window.devicePixelRatio || 1;
      const resolution = canvasResolution(width, height, ratio);
      labelCanvas.style.width = resolution.cssWidth;
      labelCanvas.style.height = resolution.cssHeight;
      if (labelCanvas.width !== resolution.pixelWidth || labelCanvas.height !== resolution.pixelHeight) {
        labelCanvas.width = resolution.pixelWidth;
        labelCanvas.height = resolution.pixelHeight;
      }
      labelContext.setTransform(ratio, 0, 0, ratio, 0, 0);
      labelContext.clearRect(0, 0, width, height);
      labelContext.fillStyle = labelColor;
      labelContext.font = "500 14px Arial";
      for (const key of renderer.getNodeDisplayedLabels()) {
        const display = renderer.getNodeDisplayData(key);
        if (!display?.label || display.visibility === "hidden") continue;
        const position = renderer.framedGraphToViewport(display);
        const lines = wrapLabel(display.label, 3);
        const size = renderer.scaleSize(display.size);
        const firstBaseline = position.y + 14 / 3 - ((lines.length - 1) * 16) / 2;
        lines.forEach((line, index) => labelContext.fillText(line, position.x + size + 3, firstBaseline + index * 16));
      }
    };
    renderer.on("afterRender", drawMemoryEdges);
    renderer.on("afterRender", drawLabels);

    renderer.on("clickNode", ({ node }) => onSelectRef.current(node));
    renderer.on("enterNode", ({ node, event }) => {
      const item = nodeByPath.get(node);
      if (item) setHover({ node: item, x: event.x, y: event.y });
    });
    renderer.on("leaveNode", () => setHover(null));

    rendererRef.current = renderer;
    graphRef.current = graph;

    let layout: FA2LayoutSupervisor<NodeAttributes, EdgeAttributes> | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    if (!cached && graph.order > 1) {
      const smallGraph = graph.order <= 12;
      layout = new FA2LayoutSupervisor(graph, {
        settings: {
          ...forceAtlas2.inferSettings(graph),
          barnesHutOptimize: graph.order > 200,
          edgeWeightInfluence: 1.2,
          gravity: smallGraph ? 1.8 : 0.7,
          scalingRatio: smallGraph ? 1.5 : 5,
          slowDown: 4,
        },
      });
      layout.start();
      timer = setTimeout(() => {
        layout?.stop();
        savePositions(data.revision, graph);
        if (graph.order <= 12) {
          const camera = renderer.getCamera();
          const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
          void camera.animate(
            { x: 0.5, y: 0.5, ratio: graph.order <= 6 ? 1.65 : 1.3 },
            { duration: reducedMotion ? 0 : 350, easing: "quadraticInOut" },
          );
        }
      }, 1600);
    }

    return () => {
      if (timer) clearTimeout(timer);
      themeObserver.disconnect();
      renderer.off("afterRender", drawMemoryEdges);
      renderer.off("afterRender", drawLabels);
      layout?.kill();
      renderer.kill();
      rendererRef.current = null;
      graphRef.current = null;
      nodeReducerRef.current = (_key, display) => display;
      edgeReducerRef.current = (_key, display) => display;
    };
  }, [data, nodeByPath]);

  useEffect(() => {
    const renderer = rendererRef.current;
    const graph = graphRef.current;
    if (!renderer || !graph) return;

    nodeReducerRef.current = (path, display, attributes) => {
      const depth = depths.get(path);
      const matchesSearch = !searchMatches || searchMatches.has(path);
      if (!matchesSearch) return { ...display, color: fadedNode, labelVisibility: "auto", zIndex: 0 };
      if (depth === 0) {
        return { ...display, size: attributes.size * 3.4, color: "#d982a3", labelVisibility: "visible", zIndex: 4 };
      }
      if (depth === 1) return { ...display, size: attributes.size * 1.22, labelVisibility: directlyRelated.has(path) ? "visible" : "auto", zIndex: 3 };
      if (depth === 2) return { ...display, size: attributes.size, labelVisibility: "auto", zIndex: 2 };
      // Selection emphasizes the local neighborhood, but the graph remains a
      // map of every wiki page. Shrinking distant nodes below Sigma's label
      // threshold and making them nearly transparent made them look absent.
      return { ...display, labelVisibility: "auto", zIndex: 0 };
    };
    edgeReducerRef.current = (edge, display, attributes) => {
      if (attributes.origin === "memory") return { ...display, visibility: "hidden" };
      const originVisible = attributes.origin === "asserted" ? showAsserted : showInferred;
      if (!originVisible) return { ...display, visibility: "hidden" };
      const [source, target] = graph.extremities(edge);
      const sourceDepth = depths.get(source);
      const targetDepth = depths.get(target);
      const touchesSelection = source === selectedPath || target === selectedPath;
      if (touchesSelection) {
        return {
          ...display,
          color: attributes.origin === "asserted" ? "rgba(80, 101, 113, 0.82)" : "rgba(80, 101, 113, 0.5)",
          label: attributes.label,
          size: attributes.size * 1.3,
          zIndex: 3,
        };
      }
      if (sourceDepth !== undefined && targetDepth !== undefined) {
        return { ...display, color: "rgba(105, 116, 128, 0.28)", label: "", zIndex: 2 };
      }
      return {
        ...display,
        color: attributes.origin === "asserted" ? ambientAssertedEdge : ambientInferredEdge,
        label: "",
        zIndex: 0,
      };
    };
    renderer.refresh();

    if (graph.hasNode(selectedPath)) {
      const display = renderer.getNodeDisplayData(selectedPath);
      if (display) {
        const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
        const camera = renderer.getCamera();
        if (graph.order <= 12) {
          void camera.animate(
            { x: 0.5, y: 0.5, ratio: graph.order <= 6 ? 1.65 : 1.3 },
            { duration: reducedMotion ? 0 : 450, easing: "quadraticInOut" },
          );
          return;
        }
        void camera.animate(
          selectionCameraTarget(display, camera.getState()),
          { duration: reducedMotion ? 0 : 450, easing: "quadraticInOut" },
        );
      }
    }
  }, [data, depths, directlyRelated, searchMatches, selectedPath, showAsserted, showInferred]);

  const zoom = (factor: number) => {
    const camera = rendererRef.current?.getCamera();
    if (!camera) return;
    void camera.animate({ ratio: camera.getState().ratio * factor }, { duration: 180 });
  };

  const reset = () => {
    void rendererRef.current?.getCamera().reset({ duration: 350 });
  };

  const goHome = () => {
    if (data.selfPath) onSelect(data.selfPath);
  };

  return (
    <div className="relative h-full min-h-0 overflow-hidden bg-[radial-gradient(circle_at_center,color-mix(in_srgb,var(--surface-tertiary)_62%,transparent),var(--surface))]">
      <div ref={containerRef} className="absolute inset-0" aria-label="Memory relationship graph" />

      <div className="absolute bottom-4 left-4 flex overflow-hidden rounded-xl border border-border bg-surface-secondary/90 shadow-sm backdrop-blur">
        <button onClick={() => zoom(0.72)} className="p-2.5 text-text-secondary hover:bg-surface-tertiary hover:text-text-primary" title="Zoom in">
          <PlusIcon className="h-4 w-4" />
        </button>
        <button onClick={() => zoom(1.38)} className="border-l border-border p-2.5 text-text-secondary hover:bg-surface-tertiary hover:text-text-primary" title="Zoom out">
          <MinusIcon className="h-4 w-4" />
        </button>
        <button onClick={reset} className="border-l border-border p-2.5 text-text-secondary hover:bg-surface-tertiary hover:text-text-primary" title="Fit graph">
          <ArrowsPointingOutIcon className="h-4 w-4" />
        </button>
        {data.selfPath && (
          <button onClick={goHome} className="border-l border-border p-2.5 text-text-secondary hover:bg-surface-tertiary hover:text-text-primary" title="Go to me">
            <HomeIcon className="h-4 w-4" />
          </button>
        )}
      </div>

      {hover && (
        <div
          className="pointer-events-none absolute z-20 w-72 rounded-xl border border-border bg-surface-secondary/95 p-3 shadow-xl backdrop-blur"
          style={{ left: Math.min(hover.x + 14, Math.max(8, (containerRef.current?.clientWidth ?? 320) - 304)), top: Math.max(8, hover.y - 12) }}
        >
          <div className="flex items-start justify-between gap-3">
            <div>
              <p className="font-semibold text-text-primary">{hover.node.name}</p>
              {hover.node.displayType && <p className="mt-0.5 text-xs text-text-tertiary">{hover.node.types.find((type) => type.iri === hover.node.displayType)?.label}</p>}
            </div>
            <span className="mt-1 h-3 w-3 shrink-0 rounded-full border-[3px] border-current bg-surface" style={{ color: colorForNode(hover.node) }} />
          </div>
          <p className="mt-2 line-clamp-2 text-sm leading-5 text-text-secondary">{hover.node.description || "No description"}</p>
          {hover.node.hoverAttributes.length > 0 && (
            <dl className="mt-2 space-y-1 border-t border-border pt-2 text-xs">
              {hover.node.hoverAttributes.map((attribute) => (
                <div key={attribute.property} className="flex justify-between gap-3">
                  <dt className="truncate text-text-tertiary">{attribute.label}</dt>
                  <dd className="max-w-[55%] truncate text-right text-text-secondary">{String(attribute.value)}</dd>
                </div>
              ))}
              {hover.node.additionalAttributeCount > 0 && <div className="text-text-tertiary">+{hover.node.additionalAttributeCount} more</div>}
            </dl>
          )}
          <p className="mt-2 text-xs text-text-tertiary">
            {hover.node.relationStats.total} relations · {hover.node.relationStats.outgoing} out · {hover.node.relationStats.incoming} in · {hover.node.relationStats.inferred} inferred
          </p>
        </div>
      )}
    </div>
  );
}
