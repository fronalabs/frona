import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { MemoryKindBadge } from "../memory-kind-badge";

describe("MemoryKindBadge", () => {
  it.each([
    ["identity", "bg-blue-500/15"],
    ["preference", "bg-purple-500/15"],
    ["fact", "bg-cyan-500/15"],
    ["reference", "bg-amber-500/15"],
    ["episodic", "bg-rose-500/15"],
    ["procedural", "bg-green-500/15"],
  ])("color codes the %s kind", (kind, expectedClass) => {
    render(<MemoryKindBadge kind={kind} />);

    const label = kind.charAt(0).toUpperCase() + kind.slice(1);
    expect(screen.getByText(label)).toHaveClass(expectedClass);
  });

  it("uses neutral styling for an unknown kind", () => {
    render(<MemoryKindBadge kind="custom" />);

    expect(screen.getByText("Custom")).toHaveClass("bg-surface-tertiary", "text-text-secondary");
  });
});
