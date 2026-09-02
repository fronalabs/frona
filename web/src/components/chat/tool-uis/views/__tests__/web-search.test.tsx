import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { SafeToolView } from "../safe-tool-view";
import { WebSearchView } from "../web-search";
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

const RESULT = `1. First Result Title
   https://example.com/first
   This is the snippet for the first result describing what it's about.

2. Second Result
   https://example.org/second
   Snippet two.`;

describe("WebSearchView", () => {
  it("renders Web Search title and the query as subtitle", () => {
    render(
      <WebSearchView
        {...mkProps({
          toolName: "web_search",
          args: { query: "react server components" },
          result: RESULT,
        })}
      />,
    );
    expect(screen.getByText("Web Search")).toBeInTheDocument();
    expect(screen.getByText(/— react server components/)).toBeInTheDocument();
  });

  it("renders each hit with title link, URL, and snippet", () => {
    render(
      <WebSearchView
        {...mkProps({
          toolName: "web_search",
          args: { query: "x" },
          result: RESULT,
        })}
      />,
    );

    const link1 = screen.getByRole("link", { name: "First Result Title" });
    expect(link1).toHaveAttribute("href", "https://example.com/first");
    expect(link1).toHaveAttribute("target", "_blank");

    expect(screen.getByText("https://example.com/first")).toBeInTheDocument();
    expect(
      screen.getByText(/This is the snippet for the first result/),
    ).toBeInTheDocument();
    expect(screen.getByText("Second Result")).toBeInTheDocument();
  });

  it('renders "No results found." for the empty case', () => {
    render(
      <WebSearchView
        {...mkProps({
          toolName: "web_search",
          args: { query: "x" },
          result: "No results found.",
        })}
      />,
    );
    expect(screen.getByText("No results found.")).toBeInTheDocument();
    expect(screen.queryByRole("link")).not.toBeInTheDocument();
  });

  it("uses the generic view when the result doesn't match the expected format", () => {
    render(
      <SafeToolView
        view={WebSearchView}
        {...mkProps({
          toolName: "web_search",
          args: { query: "x" },
          result: "Unparseable blob",
        })}
      />,
    );
    expect(screen.getByText("Result:")).toBeInTheDocument();
    expect(screen.getByText("Unparseable blob")).toBeInTheDocument();
    expect(screen.queryByRole("link")).not.toBeInTheDocument();
  });

  it("disables expansion when there's no result yet", () => {
    render(
      <WebSearchView
        {...mkProps({
          toolName: "web_search",
          args: { query: "x" },
          result: undefined,
        })}
      />,
    );
    expect(screen.getByRole("button")).toBeDisabled();
  });
});
