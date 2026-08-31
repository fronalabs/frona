import { describe, expect, it } from "vitest";

import { fileOperationPath, resolveAgentHandle } from "../file-manager-utils";
import type { Agent } from "../types";

describe("resolveAgentHandle", () => {
  it("maps a workspace display name to its agent handle", () => {
    const agents = [
      { id: "agent-id", name: "Research Agent", handle: "research-agent" },
    ] as Agent[];

    expect(
      resolveAgentHandle("/Workspaces/Research Agent/reports", agents),
    ).toBe("research-agent");
  });
});

describe("fileOperationPath", () => {
  const agents = [
    { id: "agent-id", name: "Research Agent", handle: "research-agent" },
  ] as Agent[];

  it("maps user and agent tree paths to authenticated operation paths", () => {
    expect(fileOperationPath("/My Files/reports/a.txt", "test-user", agents))
      .toBe("user://test-user/reports/a.txt");
    expect(fileOperationPath("/Workspaces/Research Agent/a.txt", "test-user", agents))
      .toBe("agent://research-agent/a.txt");
  });

  it("rejects paths outside file workspaces", () => {
    expect(fileOperationPath("/Workspaces", "test-user", agents)).toBeNull();
    expect(fileOperationPath("/unknown", "test-user", agents)).toBeNull();
  });
});
