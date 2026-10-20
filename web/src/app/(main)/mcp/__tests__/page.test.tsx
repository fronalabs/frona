import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({ get: vi.fn(), patch: vi.fn(), post: vi.fn(), delete: vi.fn() }));
vi.mock("@/lib/api-client", () => ({ api, API_URL: "", getAccessToken: () => null }));
vi.mock("next/navigation", () => ({
  useSearchParams: () => new URLSearchParams("id=server-1&section=config"),
  useRouter: () => ({ replace: vi.fn(), push: vi.fn() }),
}));
vi.mock("@/components/agents/configure/sandbox-section", () => ({ SandboxSection: () => null }));
vi.mock("@/components/agents/configure/creds-section", () => ({ AddCredentialForm: () => null }));

import Page from "../page";

beforeEach(() => vi.clearAllMocks());

it("replaces environment values and explicitly submits an empty map after deleting the final row", async () => {
  const server = {
    id: "server-1", slug: "test-server", display_name: "Test server", description: null,
    registry_id: null, status: "installed", command: "test", args: [], tool_count: 0,
    active_transport: "stdio", transports: [], sandbox_policy: {},
    installed_at: "2026-01-01T00:00:00Z", last_started_at: null,
    env: { FIRST: "one", SECOND: "two" } as Record<string, string>,
  };
  api.get.mockImplementation(async (path: string) => path === "/api/mcp/servers/server-1" ? { ...server } : []);
  api.patch.mockImplementation(async (_path: string, body: { extra_env: Record<string, string> }) => {
    server.env = body.extra_env;
    return { restart_required: false };
  });
  const view = render(<Page />);
  fireEvent.click(await screen.findByRole("button", { name: "Remove FIRST" }));
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(api.patch).toHaveBeenLastCalledWith("/api/mcp/servers/server-1", expect.objectContaining({ extra_env: { SECOND: "two" } })));
  await waitFor(() => expect(screen.getByRole("button", { name: "Save" })).toBeDisabled());
  fireEvent.click(screen.getByRole("button", { name: "Remove SECOND" }));
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(api.patch).toHaveBeenLastCalledWith("/api/mcp/servers/server-1", expect.objectContaining({ extra_env: {} })));
  await waitFor(() => expect(screen.getByRole("button", { name: "Save" })).toBeDisabled());
  expect(api.delete).not.toHaveBeenCalled();
  view.unmount();
  render(<Page />);
  await screen.findByText("Test server");
  expect(screen.queryByDisplayValue("FIRST")).not.toBeInTheDocument();
  expect(screen.queryByDisplayValue("SECOND")).not.toBeInTheDocument();
});
