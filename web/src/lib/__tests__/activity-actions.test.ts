import { describe, expect, it } from "vitest";
import { executionHref } from "../activity-actions";
import type { Execution } from "../types";

describe("executionHref", () => {
  it("opens memory consolidation progress for a memory execution", () => {
    const execution: Execution = {
      id: "memory-execution",
      title: "Memory consolidation",
      kind: "memory",
      status: "running",
      action: "Consolidating memory",
      source: { type: "system" },
      startedAt: "2026-09-03T00:00:00Z",
      canCancel: false,
    };

    expect(executionHref(execution)).toBe("/memory?consolidation=expanded");
  });
});
