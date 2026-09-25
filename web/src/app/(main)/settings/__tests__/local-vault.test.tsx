import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import SettingsPage from "../page";
import { api, listVaultConnections } from "@/lib/api-client";

vi.mock("@/lib/api-client", () => ({
  api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() },
  listVaultConnections: vi.fn(), createVaultConnection: vi.fn(), deleteVaultConnection: vi.fn(),
  toggleVaultConnection: vi.fn(), testVaultConnection: vi.fn(),
}));
vi.mock("@/lib/use-mobile", () => ({ useMobile: () => false }));
vi.mock("@/lib/navigation-context", () => ({
  useNavigation: () => ({ mobileSubNavOpen: false, setMobileSubNavOpen: vi.fn() }),
}));
vi.mock("@/components/settings/sections/profile-section", () => ({ ProfileSection: () => null }));
vi.mock("@/components/settings/sections/mcp-section", () => ({ McpSection: () => null }));
vi.mock("@/components/settings/sections/channels-section", () => ({ ChannelsSection: () => null }));
vi.mock("@/components/settings/sections/skills-section", () => ({ SkillsSection: () => null }));
vi.mock("@/components/settings/sections/user-memory-section", () => ({ UserMemorySection: () => null }));

describe("saving local vault secrets from settings", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    window.history.replaceState(null, "", "/settings#vault");
    vi.mocked(listVaultConnections).mockResolvedValue([]);
    vi.mocked(api.get).mockResolvedValue([]);
    vi.mocked(api.post).mockImplementation(async () => {
      const saved = { id: "saved-key", name: "Test service", data: { type: "ApiKey", data: {} } };
      vi.mocked(api.get).mockResolvedValue([saved]);
      return saved;
    });
  });

  it("lets a user create and save a new API key", async () => {
    render(<SettingsPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Add" }));
    fireEvent.click(screen.getByRole("button", { name: "API Key" }));
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: "Test service" } });
    fireEvent.change(screen.getByPlaceholderText("API Key"), { target: { value: "test-only-placeholder" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/vaults/local/items", {
      type: "ApiKey", name: "Test service", api_key: "test-only-placeholder",
    }));
    await waitFor(() => expect(screen.queryByText("New — will be created on save")).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: /Test service.*API Key/ })).toBeInTheDocument();
  });

  it("keeps a new secret after a failed save so it can be retried", async () => {
    vi.mocked(api.post).mockRejectedValueOnce(new Error("Vault is unavailable"));
    render(<SettingsPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Add" }));
    fireEvent.click(screen.getByRole("button", { name: "API Key" }));
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: "Test service" } });
    fireEvent.change(screen.getByPlaceholderText("API Key"), { target: { value: "test-only-placeholder" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Vault is unavailable");
    expect(screen.getByPlaceholderText("API Key")).toHaveValue("test-only-placeholder");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("alert")).not.toBeInTheDocument());
    await waitFor(() => expect(screen.queryByText("New — will be created on save")).not.toBeInTheDocument());
    expect(api.post).toHaveBeenCalledTimes(2);
  });

  it("keeps incomplete drafts and lets the user discard them", async () => {
    render(<SettingsPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Add" }));
    fireEvent.click(screen.getByRole("button", { name: "API Key" }));
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: "Test service" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("required fields");
    expect(screen.getByPlaceholderText("Name")).toHaveValue("Test service");
    expect(api.post).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(screen.queryByText("Test service")).not.toBeInTheDocument());
  });

  it("saves a username and password", async () => {
    render(<SettingsPage />);
    fireEvent.click(await screen.findByRole("button", { name: "Add" }));
    fireEvent.click(screen.getByRole("button", { name: "Password" }));
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: "Test login" } });
    fireEvent.change(screen.getByPlaceholderText("Username"), { target: { value: "test-user" } });
    fireEvent.change(screen.getByPlaceholderText("Password"), { target: { value: "test-only-placeholder" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/vaults/local/items", {
      type: "UsernamePassword", name: "Test login", username: "test-user", password: "test-only-placeholder",
    }));
  });

  it("does not recreate an already saved secret when a later save fails", async () => {
    vi.mocked(api.post)
      .mockResolvedValueOnce({ id: "first", name: "First key", data: { type: "ApiKey", data: {} } })
      .mockRejectedValueOnce(new Error("Vault is unavailable"))
      .mockResolvedValueOnce({ id: "second", name: "Second key", data: { type: "ApiKey", data: {} } });
    render(<SettingsPage />);
    for (const name of ["First key", "Second key"]) {
      fireEvent.click(await screen.findByRole("button", { name: "Add" }));
      fireEvent.click(screen.getByRole("button", { name: "API Key" }));
      fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: name } });
      fireEvent.change(screen.getByPlaceholderText("API Key"), { target: { value: "test-only-placeholder" } });
    }
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Vault is unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument());
    expect(api.post).toHaveBeenCalledTimes(3);
    expect(api.post).toHaveBeenLastCalledWith("/api/vaults/local/items", {
      type: "ApiKey", name: "Second key", api_key: "test-only-placeholder",
    });
    expect(screen.getByRole("button", { name: /First key.*API Key/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Second key.*API Key/ })).toBeInTheDocument();
  });

  it("saves changes to an existing secret without overwriting its hidden value", async () => {
    const saved = { id: "existing", name: "Existing key", data: { type: "ApiKey", data: {} } };
    vi.mocked(api.get).mockResolvedValue([saved]);
    vi.mocked(api.put).mockResolvedValue({ ...saved, name: "Renamed key" });
    render(<SettingsPage />);
    fireEvent.click(await screen.findByRole("button", { name: /Existing key.*API Key/ }));
    fireEvent.change(screen.getByPlaceholderText("Name"), { target: { value: "Renamed key" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.put).toHaveBeenCalledWith("/api/vaults/local/items/existing", {
      type: "ApiKey", name: "Renamed key",
    }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument());
    expect(api.post).not.toHaveBeenCalled();
  });
});
