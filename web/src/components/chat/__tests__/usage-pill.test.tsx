import React from "react";
import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { UsagePill } from "../usage-pill";

it("uses the resolved model window for context usage independently of cumulative usage", () => {
  const totals = { inputTokens: 2_000_000, cachedInputTokens: 0, outputTokens: 1000, costUsd: 1, calls: 10 };
  const { rerender } = render(<UsagePill totals={totals} currentInputTokens={450_000} contextWindow={900_000} />);
  expect(screen.getByText("450k/900k (50%)")).toBeInTheDocument();

  rerender(<UsagePill totals={totals} currentInputTokens={450_000} contextWindow={600_000} />);
  expect(screen.getByText("450k/600k (75%)")).toBeInTheDocument();
});
