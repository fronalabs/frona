import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { UserVaultSection } from "../vault-section";
import { api, createVaultConnection, deleteVaultConnection, listVaultConnections, testVaultConnection, toggleVaultConnection, type VaultConnection } from "@/lib/api-client";

vi.mock("@/lib/api-client", () => ({
  api: { get: vi.fn(), post: vi.fn() },
  listVaultConnections: vi.fn(), createVaultConnection: vi.fn(),
  deleteVaultConnection: vi.fn(), toggleVaultConnection: vi.fn(), testVaultConnection: vi.fn(),
}));
vi.mock("@/components/settings/settings-context", () => ({
  useSettings: () => ({ setModified: vi.fn(), register: vi.fn(), unregister: vi.fn() }),
}));

const personal: VaultConnection = {
  id: "personal-1", name: "Personal keys", provider: "managed",
  enabled: true, system_managed: false, created_at: "", updated_at: "",
};

describe("personal managed vault connections", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(api.get).mockResolvedValue([]);
    vi.mocked(api.post).mockResolvedValue({ status: "ok" });
    vi.mocked(listVaultConnections).mockResolvedValue([]);
    vi.mocked(createVaultConnection).mockResolvedValue(personal);
    vi.mocked(deleteVaultConnection).mockResolvedValue(undefined);
    vi.mocked(testVaultConnection).mockResolvedValue(undefined);
    vi.mocked(toggleVaultConnection).mockResolvedValue({ ...personal, enabled: false });
    vi.spyOn(window, "confirm").mockReturnValue(true);
  });

  it("creates a named managed connection without namespace or ownership fields", async () => {
    render(<UserVaultSection />);
    fireEvent.click(await screen.findByRole("button", { name: "Add connection" }));
    fireEvent.click(screen.getByRole("button", { name: "Managed" }));
    fireEvent.change(screen.getByPlaceholderText("My Managed"), { target: { value: "Personal keys" } });
    fireEvent.blur(screen.getByPlaceholderText("My Managed"));
    expect(screen.queryByLabelText(/namespace|global|scope/i)).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("button", { name: "Create" })).toBeEnabled(), { timeout: 2000 });
    expect(api.post).toHaveBeenCalledWith("/api/vaults/test", { provider: "managed", config: { type: "Managed" } });
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(createVaultConnection).toHaveBeenCalledWith({
      name: "Personal keys", provider: "managed", config: { type: "Managed" },
    }));
  });

  it("uses ordinary personal connection controls and reports blocked deletion", async () => {
    vi.mocked(listVaultConnections).mockResolvedValue([
      personal, { ...personal, id: "managed", name: "Shared keys", system_managed: true },
    ]);
    render(<UserVaultSection />);
    expect(await screen.findByText("Personal keys")).toBeInTheDocument();
    expect(screen.queryByText("Shared keys")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Disable Personal keys" }));
    await waitFor(() => expect(toggleVaultConnection).toHaveBeenCalledWith("personal-1", false));
    fireEvent.click(screen.getByRole("button", { name: "Actions for Personal keys" }));
    fireEvent.click(screen.getByRole("button", { name: "Test" }));
    await waitFor(() => expect(testVaultConnection).toHaveBeenCalledWith("personal-1"));
    vi.mocked(deleteVaultConnection).mockRejectedValueOnce(new Error("Delete credentials before deleting this vault"));
    fireEvent.click(screen.getByRole("button", { name: "Actions for Personal keys" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Delete credentials before deleting this vault");
    expect(screen.getByText("Personal keys")).toBeInTheDocument();
    vi.mocked(listVaultConnections).mockResolvedValue([]);
    fireEvent.click(screen.getByRole("button", { name: "Actions for Personal keys" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.queryByText("Personal keys")).not.toBeInTheDocument());
  });
});
