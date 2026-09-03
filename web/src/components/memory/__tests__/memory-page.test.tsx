import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  getMemoryGraph: vi.fn(),
  getPkmStatus: vi.fn(),
  searchMemory: vi.fn(),
}));
const inspector = vi.hoisted(() => ({ props: vi.fn() }));
const navigation = vi.hoisted(() => ({ search: "" }));

vi.mock("@/lib/api-client", () => api);
vi.mock("@/lib/use-mobile", () => ({ useMobile: () => false }));
vi.mock("next/navigation", () => ({
  usePathname: () => "/memory",
  useSearchParams: () => new URLSearchParams(navigation.search),
}));
vi.mock("../memory-graph-canvas", () => ({ MemoryGraphCanvas: () => null }));
vi.mock("../memory-inspector", () => ({
  MemoryInspector: (props: unknown) => {
    inspector.props(props);
    return null;
  },
}));

import { MemoryPage } from "../memory-page";

describe("MemoryPage", () => {
  beforeEach(() => {
    api.getMemoryGraph.mockReset();
    api.getPkmStatus.mockReset();
    api.getPkmStatus.mockResolvedValue({ available: true, reset: null, consolidation: null });
    api.searchMemory.mockReset();
    inspector.props.mockReset();
    navigation.search = "";
  });

  it("shows an empty state after a new user's empty graph loads", async () => {
    api.getMemoryGraph.mockResolvedValue({
      revision: "empty",
      selfPath: null,
      nodes: [],
      edges: [],
      legend: [],
    });

    render(<MemoryPage />);

    expect(screen.getByText("Loading memory graph…")).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "No memories yet" })).toBeInTheDocument();
    expect(screen.queryByText("Loading memory graph…")).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Start a conversation" })).toHaveAttribute("href", "/");
  });

  it("opens the sidebar with the existing pages ready to browse", async () => {
    api.getMemoryGraph.mockResolvedValue({
      revision: "one",
      selfPath: "people/zoe",
      nodes: [
        {
          path: "projects/beta",
          name: "Beta",
          description: "A project",
          useCount: 8,
          origin: "internal",
          category: "concept",
          types: [],
          displayType: null,
          colorBranch: "",
          hoverAttributes: [],
          additionalAttributeCount: 0,
          relationStats: { total: 0, incoming: 0, outgoing: 0, asserted: 0, inferred: 0 },
        },
        {
          path: "people/zoe",
          name: "Zoe",
          description: "The owner",
          useCount: 0,
          origin: "internal",
          category: "concept",
          types: [{ iri: "Person", label: "Person", ancestors: [] }],
          displayType: "Person",
          colorBranch: "Person",
          hoverAttributes: [],
          additionalAttributeCount: 0,
          relationStats: { total: 0, incoming: 0, outgoing: 0, asserted: 0, inferred: 0 },
        },
        {
          path: "people/alice",
          name: "Alice",
          description: "A person",
          useCount: 3,
          origin: "internal",
          category: "concept",
          types: [],
          displayType: null,
          colorBranch: "",
          hoverAttributes: [],
          additionalAttributeCount: 0,
          relationStats: { total: 0, incoming: 0, outgoing: 0, asserted: 0, inferred: 0 },
        },
      ],
      edges: [],
      legend: [],
    });

    render(<MemoryPage />);

    await screen.findByPlaceholderText("Search 3 memory pages");
    expect(inspector.props).toHaveBeenLastCalledWith(expect.objectContaining({
      open: true,
      searchMode: true,
      browsablePages: [
        expect.objectContaining({ path: "people/zoe", name: "Zoe" }),
        expect.objectContaining({ path: "projects/beta", name: "Beta" }),
        expect.objectContaining({ path: "people/alice", name: "Alice" }),
      ],
    }));
  });

  it("opens consolidation progress when requested by the activity link", async () => {
    navigation.search = "consolidation=expanded";
    api.getMemoryGraph.mockResolvedValue({
      revision: "one",
      selfPath: "people/zoe",
      nodes: [
        {
          path: "people/zoe",
          name: "Zoe",
          description: "The owner",
          useCount: 0,
          origin: "internal",
          category: "concept",
          types: [],
          displayType: null,
          colorBranch: "",
          hoverAttributes: [],
          additionalAttributeCount: 0,
          relationStats: { total: 0, incoming: 0, outgoing: 0, asserted: 0, inferred: 0 },
        },
      ],
      edges: [],
      legend: [],
    });

    render(<MemoryPage />);

    const progressButton = await screen.findByRole("button", {
      name: "Memory consolidation status",
    });
    expect(progressButton).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/No consolidation has run yet/)).toBeInTheDocument();
  });
});
