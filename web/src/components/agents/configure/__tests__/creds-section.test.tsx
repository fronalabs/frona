import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api } from "@/lib/api-client";
import { AddCredentialForm, type VaultConnection, type VaultGrant } from "../creds-section";

vi.mock("@/lib/api-client", () => ({ api: { get: vi.fn(), post: vi.fn() } }));

const connections = new Map<string, VaultConnection>([
  ["local", { id: "local", name: "Local vault", provider: "local", enabled: true }],
  ["work", { id: "work", name: "Work vault", provider: "one_password", enabled: true }],
  ["disabled", { id: "disabled", name: "Disabled vault", provider: "bitwarden", enabled: false }],
]);
const localItem = { id: "shared-id", name: "Local login", username: "alice" };
const workItem = { id: "shared-id", name: "Work login", username: "bob" };

function renderPicker(props: Partial<React.ComponentProps<typeof AddCredentialForm>> = {}) {
  const onCreated = vi.fn();
  render(<AddCredentialForm connections={connections} principalKind="agent" principalId="agent-1"
    existingGrants={[]} onClose={vi.fn()} onCreated={onCreated} {...props} />);
  return { onCreated };
}

function pending<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

describe("unified credential picker", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(api.get).mockImplementation(async (path) => {
      if (path.endsWith("/fields")) return ["USERNAME", "PASSWORD"];
      return path.startsWith("/api/vaults/local/") ? [localItem] : [workItem];
    });
    vi.mocked(api.post).mockResolvedValue({ id: "grant-1" });
  });

  it("searches every enabled vault and grants the selected item from its own vault", async () => {
    const { onCreated } = renderPicker();
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    expect(await screen.findByRole("button", { name: /Local login, Local vault/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Work login, Work vault/ })).toBeInTheDocument();
    expect(api.get).not.toHaveBeenCalledWith(expect.stringContaining("/disabled/"));
    expect(screen.getByRole("button", { name: "Add" })).toBeDisabled();

    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "login & key" } });
    await waitFor(() => {
      expect(api.get).toHaveBeenCalledWith("/api/vaults/local/items?q=login%20%26%20key");
      expect(api.get).toHaveBeenCalledWith("/api/vaults/work/items?q=login%20%26%20key");
    });
    fireEvent.click(await screen.findByRole("button", { name: /Work login, Work vault/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Add" })).toBeEnabled());
    expect(screen.getByRole("button", { name: /Local login, Local vault/ })).toHaveAttribute("aria-pressed", "false");
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/vaults/grants", {
      principal: { kind: "agent", id: "agent-1" }, connection_id: "work", vault_item_id: "shared-id",
      query: "WORK_LOGIN", target: { Prefix: { env_var_prefix: "WORK_LOGIN" } },
    }));
    expect(onCreated).toHaveBeenCalledWith({ id: "grant-1" });
  });

  it("checks existing grants using both the vault and item ID", async () => {
    renderPicker({ existingGrants: [{ connection_id: "local", vault_item_id: "shared-id" } as VaultGrant] });
    expect(await screen.findByRole("button", { name: /Local login/ })).toBeDisabled();
    expect(screen.getByRole("button", { name: /Work login/ })).toBeEnabled();
  });

  it("keeps successful results when a vault fails and allows retry", async () => {
    vi.mocked(api.get).mockImplementation(async (path) => {
      if (path.startsWith("/api/vaults/work/")) throw new Error("Unavailable");
      return [localItem];
    });
    renderPicker();
    expect(await screen.findByRole("button", { name: /Local login/ })).toBeEnabled();
    expect(screen.getByRole("alert")).toHaveTextContent("Could not search: Work vault");
    vi.mocked(api.get).mockResolvedValue([workItem]);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("button", { name: /Work login, Work vault/ })).toBeEnabled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("ignores search results from an older query", async () => {
    const oldSearch = pending<typeof localItem[]>();
    vi.mocked(api.get).mockImplementation(async (path) => path.endsWith("q=") ? oldSearch.promise : [workItem]);
    renderPicker();
    await waitFor(() => expect(api.get).toHaveBeenCalledWith("/api/vaults/work/items?q="));
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "work" } });
    expect(await screen.findByRole("button", { name: /Work login, Work vault/ })).toBeInTheDocument();
    await act(async () => oldSearch.resolve([localItem]));
    expect(screen.queryByRole("button", { name: /Local login/ })).not.toBeInTheDocument();
  });

  it("ignores stale fields when switching vaults and preserves deferred bindings", async () => {
    const localFields = pending<string[]>();
    vi.mocked(api.get).mockImplementation(async (path) => {
      if (path === "/api/vaults/local/items/shared-id/fields") return localFields.promise;
      if (path.endsWith("/fields")) return ["API_KEY"];
      return path.startsWith("/api/vaults/local/") ? [localItem] : [workItem];
    });
    const deferred = vi.fn();
    renderPicker({ targetEnvVar: "SERVICE_TOKEN", deferred });
    fireEvent.click(await screen.findByRole("button", { name: /Local login/ }));
    expect(screen.getByRole("button", { name: "Add" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: /Work login/ }));
    expect(await screen.findByRole("button", { name: "api_key" })).toBeInTheDocument();
    await act(async () => localFields.resolve(["PASSWORD"]));
    expect(screen.queryByRole("button", { name: "password" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    expect(deferred).toHaveBeenCalledWith(expect.objectContaining({
      connection_id: "work", vault_item_id: "shared-id", item_name: "Work login", connection_name: "Work vault",
      fields: ["API_KEY"], target: { Single: { env_var: "SERVICE_TOKEN", field: { Custom: { name: "API_KEY" } } } },
    }));
    expect(api.post).not.toHaveBeenCalled();
  });

  it("preserves an initial selection from a non-default vault", async () => {
    renderPicker({ initialSelection: { connection_id: "work", vault_item_id: "shared-id" }, targetEnvVar: "TOKEN" });
    expect(await screen.findByRole("button", { name: /Work login/ })).toHaveAttribute("aria-pressed", "true");
    expect(api.get).toHaveBeenCalledWith("/api/vaults/work/items/shared-id/fields");
    expect(screen.getByRole("button", { name: "Add" })).toBeEnabled();
  });

  it("blocks submission on field failure and retries selection", async () => {
    vi.mocked(api.get).mockImplementation(async (path) => {
      if (path.endsWith("/fields")) throw new Error("Unavailable");
      return [localItem];
    });
    renderPicker({ targetEnvVar: "TOKEN" });
    fireEvent.click(await screen.findByRole("button", { name: /Local login, Local vault/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Could not load credential fields");
    expect(screen.getByRole("button", { name: "Add" })).toBeDisabled();
    vi.mocked(api.get).mockResolvedValue(["PASSWORD"]);
    fireEvent.click(screen.getByRole("button", { name: /Local login, Local vault/ }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Add" })).toBeEnabled());
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("selects a newly created local credential and clears the previous search", async () => {
    renderPicker();
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "unrelated" } });
    fireEvent.click(screen.getByRole("button", { name: "New" }));
    fireEvent.change(screen.getByPlaceholderText("e.g. Google OAuth"), { target: { value: "New key" } });
    fireEvent.change(screen.getByPlaceholderText("API Key"), { target: { value: "secret" } });
    const created = { id: "new-key", name: "New key", username: null };
    vi.mocked(api.post).mockResolvedValue(created);
    vi.mocked(api.get).mockImplementation(async (path) => path.endsWith("/fields") ? ["API_KEY"] : [created]);
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    expect(await screen.findByRole("searchbox")).toHaveValue("");
    expect(await screen.findByRole("button", { name: /New key, Local vault/ })).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/vaults/grants", expect.objectContaining({
      connection_id: "local", vault_item_id: "new-key", query: "NEW_KEY",
    })));
  });

  it("explains when there are no enabled vaults", () => {
    renderPicker({ connections: new Map() });
    expect(screen.getByText(/No enabled vaults/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "New" })).not.toBeInTheDocument();
  });
});
