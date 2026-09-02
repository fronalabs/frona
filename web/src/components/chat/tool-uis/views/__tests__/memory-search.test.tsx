import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemorySearchView } from "../memory-search";
import { SafeToolView } from "../safe-tool-view";
import { mkProps } from "./helpers";

vi.mock("motion/react", () => ({
  AnimatePresence: ({ children }: { children: React.ReactNode }) => <>{children}</>,
  motion: new Proxy({}, {
    get: () => (props: Record<string, unknown>) => {
      const { children, ...rest } = props as { children?: React.ReactNode } & Record<string, unknown>;
      const filtered: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(rest)) {
        if (k === "initial" || k === "animate" || k === "exit" || k === "transition") continue;
        filtered[k] = v;
      }
      return <div {...filtered}>{children}</div>;
    },
  }),
}));

const RESULT = `Top matches - read(<path>) to open one:

- Mina  [person]
  Mina is a software engineer at Amazon, originally from Egypt and living in Atherton, California.
  /app/data/users/mina/pkm/Memory/people/me.md

- Football  [Topic]
  Mina follows football (soccer) and is currently tracking the World Cup.
  /app/data/users/mina/pkm/Memory/topic/football.md`;

const STRUCTURED_RESULT_VALUE = {
  query: "Mina",
  results: [
    {
      path: "people/me",
      name: "Mina",
      description: "The account owner.",
      origin: "internal",
      category: "concept",
      matched_by: [
        { kind: "exact_name" },
        { kind: "body_text", snippet: "Mina approved the Postgres migration." },
      ],
      file: "/app/data/users/mina/pkm/Memory/people/me.md",
    },
  ],
  truncated: false,
};
const STRUCTURED_RESULT = JSON.stringify(STRUCTURED_RESULT_VALUE);

describe("MemorySearchView", () => {
  it("renders the structured memory_search response", () => {
    render(
      <MemorySearchView
        {...mkProps({
          toolName: "memory_search",
          args: { query: "Mina" },
          result: STRUCTURED_RESULT,
        })}
      />,
    );

    expect(screen.getByText(/Mina - 1 match/)).toBeInTheDocument();
    expect(screen.getByText("concept")).toBeInTheDocument();
    expect(screen.getByText("The account owner.")).toBeInTheDocument();
    expect(screen.getByText("Mina approved the Postgres migration.")).toBeInTheDocument();
    expect(screen.getByText("people/me.md")).toBeInTheDocument();
    expect(screen.queryByText(STRUCTURED_RESULT_VALUE.results[0].file)).not.toBeInTheDocument();
  });

  it("accepts a decoded structured response and marks a truncated count", () => {
    render(
      <MemorySearchView
        {...mkProps({
          toolName: "memory_search",
          args: { query: "Mina" },
          result: { ...STRUCTURED_RESULT_VALUE, truncated: true },
        })}
      />,
    );

    expect(screen.getByText(/Mina - 1\+ matches/)).toBeInTheDocument();
    expect(screen.getByText("Mina approved the Postgres migration.")).toBeInTheDocument();
  });

  it("renders an empty structured response as no matches", () => {
    render(
      <MemorySearchView
        {...mkProps({
          toolName: "memory_search",
          args: { query: "unknown" },
          result: JSON.stringify({ query: "unknown", results: [], truncated: false }),
        })}
      />,
    );

    expect(screen.getByText("No pages matched.")).toBeInTheDocument();
  });

  it("renders Memory Search title and a query + count subtitle", () => {
    render(
      <MemorySearchView
        {...mkProps({ toolName: "memory_search", args: { query: "Mina" }, result: RESULT })}
      />,
    );
    expect(screen.getByText("Memory Search")).toBeInTheDocument();
    expect(screen.getByText(/Mina - 2 matches/)).toBeInTheDocument();
  });

  it("renders each hit with name, tag, description, and path", () => {
    render(
      <MemorySearchView
        {...mkProps({ toolName: "memory_search", args: { query: "Mina" }, result: RESULT })}
      />,
    );
    expect(screen.getByText("Mina")).toBeInTheDocument();
    expect(screen.getByText("person")).toBeInTheDocument();
    expect(screen.getByText(/software engineer at Amazon/)).toBeInTheDocument();
    expect(screen.getByText("people/me.md")).toBeInTheDocument();

    expect(screen.getByText("Football")).toBeInTheDocument();
    expect(screen.getByText("Topic")).toBeInTheDocument();
    expect(screen.getByText("topic/football.md")).toBeInTheDocument();
  });

  it('renders "No pages matched." for the empty case', () => {
    render(
      <MemorySearchView
        {...mkProps({
          toolName: "memory_search",
          args: { query: "x" },
          result: "No pages matched. The KB doesn't model this yet - ask the user, or reformulate.",
        })}
      />,
    );
    expect(screen.getByText("No pages matched.")).toBeInTheDocument();
  });

  it("uses the generic view when the result doesn't match the expected format", () => {
    const oldResult = JSON.stringify({ results: [{ name: "Mina" }] });
    render(
      <SafeToolView
        view={MemorySearchView}
        {...mkProps({ toolName: "memory_search", args: { query: "x" }, result: oldResult })}
      />,
    );
    expect(screen.getByText("Result:")).toBeInTheDocument();
    expect(screen.getByText(oldResult)).toBeInTheDocument();
  });

  it("disables expansion when there's no result yet", () => {
    render(
      <MemorySearchView
        {...mkProps({ toolName: "memory_search", args: { query: "x" }, result: undefined })}
      />,
    );
    expect(screen.getByRole("button")).toBeDisabled();
  });
});
