import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MemoryGraphSparqlView } from "../memory-graph-sparql";
import { pickView } from "../index";
import { SafeToolView } from "../safe-tool-view";
import { mkProps } from "./helpers";

vi.mock("motion/react", () => ({
  AnimatePresence: ({ children }: { children: React.ReactNode }) => <>{children}</>,
  motion: new Proxy(
    {},
    {
      get: () => (props: Record<string, unknown>) => {
        const { children, ...rest } = props as { children?: React.ReactNode } & Record<
          string,
          unknown
        >;
        const filtered: Record<string, unknown> = {};
        for (const [key, value] of Object.entries(rest)) {
          if (["initial", "animate", "exit", "transition"].includes(key)) continue;
          filtered[key] = value;
        }
        return <div {...filtered}>{children}</div>;
      },
    },
  ),
}));

vi.mock("@/components/ui/code-block", () => ({
  CodeBlock: ({
    code,
    language,
    wrap,
  }: {
    code: string;
    language?: string;
    wrap?: boolean;
  }) => (
    <pre data-testid="code-block" data-lang={language ?? ""} data-wrap={wrap ? "1" : "0"}>
      {code}
    </pre>
  ),
}));

vi.mock("sparql-formatter", () => ({
  spfmt: {
    format: (query: string) => {
      if (query === "INVALID") throw new Error("parse error");
      if (query === QUERY) {
        return "SELECT ?person ?type\nWHERE {\n  ?person a schema:Person ;\n          a ?type .\n}";
      }
      return query;
    },
  },
}));

const QUERY = "SELECT ?person ?type WHERE { ?person a schema:Person; a ?type }";

describe("MemoryGraphSparqlView", () => {
  it("is registered for memory_graph_sparql", () => {
    expect(pickView("memory_graph_sparql")).toBe(MemoryGraphSparqlView);
  });

  it("formats and highlights the query, then renders rows in backend column order", async () => {
    render(
      <MemoryGraphSparqlView
        {...mkProps({
          toolName: "memory_graph_sparql",
          args: { query: QUERY },
          result: JSON.stringify({
            kind: "solutions",
            columns: ["person", "type"],
            rows: [
              { person: "people/mina", type: "schema:Person" },
              { person: "people/sarah" },
            ],
            truncated: false,
          }),
        })}
      />,
    );

    expect(screen.getByText("Memory Graph Query")).toBeInTheDocument();
    expect(screen.getByText(/2 rows/)).toBeInTheDocument();
    expect(screen.getByTestId("code-block")).toHaveAttribute("data-lang", "sparql");
    expect(screen.getByTestId("code-block")).toHaveAttribute("data-wrap", "1");
    await waitFor(() => {
      expect(screen.getByTestId("code-block")).toHaveTextContent(
        "SELECT ?person ?type WHERE { ?person a schema:Person ; a ?type . }",
      );
    });
    expect(screen.getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual([
      "?person",
      "?type",
    ]);
    expect(screen.getByText("people/mina")).toBeInTheDocument();
    expect(screen.getByText("schema:Person")).toBeInTheDocument();
    expect(screen.getByText("unbound")).toBeInTheDocument();
  });

  it("renders ASK results without a table", () => {
    render(
      <MemoryGraphSparqlView
        {...mkProps({
          toolName: "memory_graph_sparql",
          args: { query: "ASK { <urn:frona:kb:people/mina> a schema:Person }" },
          result: { kind: "boolean", value: true },
        })}
      />,
    );

    expect(screen.getByText(/ASK true/)).toBeInTheDocument();
    expect(screen.getByText("true")).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("marks truncated solution counts and explains the omitted rows", () => {
    render(
      <MemoryGraphSparqlView
        {...mkProps({
          toolName: "memory_graph_sparql",
          args: { query: QUERY },
          result: {
            kind: "solutions",
            columns: ["person"],
            rows: [{ person: "people/mina" }],
            truncated: true,
          },
        })}
      />,
    );

    expect(screen.getByText(/1\+ rows/)).toBeInTheDocument();
    expect(screen.getByText("More rows not shown.")).toBeInTheDocument();
  });

  it("uses the generic view for an unknown result shape", () => {
    render(
      <SafeToolView
        view={MemoryGraphSparqlView}
        {...mkProps({
          toolName: "memory_graph_sparql",
          args: { query: QUERY },
          result: "not-json",
        })}
      />,
    );

    expect(screen.getByText("Result:")).toBeInTheDocument();
    expect(screen.getByText("not-json")).toBeInTheDocument();
  });

  it("keeps the highlighted query and error text on failure", () => {
    render(
      <MemoryGraphSparqlView
        {...mkProps({
          toolName: "memory_graph_sparql",
          args: { query: QUERY },
          status: { type: "incomplete", reason: "error", error: "Unknown prefix" },
        })}
      />,
    );

    expect(screen.getByTestId("code-block")).toHaveAttribute("data-lang", "sparql");
    expect(screen.getByText("SPARQL query failed")).toBeInTheDocument();
    expect(screen.getByText("Unknown prefix")).toBeInTheDocument();
  });

  it("keeps the original query when formatting fails", async () => {
    render(
      <MemoryGraphSparqlView
        {...mkProps({
          toolName: "memory_graph_sparql",
          args: { query: "INVALID" },
          result: { kind: "boolean", value: false },
        })}
      />,
    );

    await waitFor(() => {
      expect(screen.getByTestId("code-block")).toHaveTextContent("INVALID");
    });
  });
});
