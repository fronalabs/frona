import { describe, expect, it } from "vitest";

import {
  fileBrowserAncestors,
  fileOperationPath,
  resolveAgentHandle,
  toSvarEntries,
} from "../file-manager-utils";
import type { Agent, FileEntry } from "../types";

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

describe("toSvarEntries", () => {
  it("keeps nested agent entries attached to the requested folder", () => {
    const entries = [{
      id: "/reports/weekly",
      parent: "/reports",
      type: "folder",
      size: 0,
      date: new Date("2026-01-01T00:00:00Z"),
    }] as FileEntry[];

    expect(toSvarEntries(entries, "/Workspaces/Test Agent")).toEqual([
      expect.objectContaining({
        id: "/Workspaces/Test Agent/reports/weekly",
        parent: "/Workspaces/Test Agent/reports",
      }),
    ]);
  });
});

describe("fileBrowserAncestors", () => {
  it("returns each folder needed to restore a nested workspace path", () => {
    expect(fileBrowserAncestors("/Workspaces/Test Agent/reports/weekly")).toEqual([
      "/Workspaces",
      "/Workspaces/Test Agent",
      "/Workspaces/Test Agent/reports",
      "/Workspaces/Test Agent/reports/weekly",
    ]);
  });

  it("rejects paths outside the file browser roots", () => {
    expect(fileBrowserAncestors("/")).toEqual([]);
    expect(fileBrowserAncestors("/unknown/folder")).toEqual([]);
  });
});
