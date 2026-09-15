import React from "react";
import { beforeEach, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import AdminSettingsPage from "@/app/(main)/admin/settings/page";
import SetupPage from "@/app/setup/page";
import { api } from "@/lib/api-client";

vi.mock("@/lib/api-client", async importOriginal => ({ ...await importOriginal<object>(), api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() } }));
vi.mock("next/navigation", () => ({ useRouter: () => ({ replace: vi.fn(), push: vi.fn() }) }));
vi.mock("@/lib/auth", () => ({ useAuth: () => ({ user: { permissions: { is_admin: true, list_users: true } } }) }));
vi.mock("@/lib/use-mobile", () => ({ useMobile: () => false }));
vi.mock("@/lib/navigation-context", () => ({ useNavigation: () => ({ mobileSubNavOpen: false, setMobileSubNavOpen: vi.fn() }) }));
vi.mock("@/components/require-auth", () => ({ RequireAuth: ({ children }: { children: React.ReactNode }) => children }));
vi.mock("@/components/settings/sections/models-section", () => ({ ModelsSection: ({ models, onChange }: {
  models: Record<string, import("@/lib/config-types").ModelGroupConfig>;
  onChange: (models: Record<string, import("@/lib/config-types").ModelGroupConfig>) => void;
}) => <><p>Model step</p><button onClick={() => {
  const next = { ...models.primary }; delete next.temperature;
  onChange({ ...models, primary: { ...next, extra_params: { complex: { nested: [null] }, literal: null } } });
}}>Edit model fixture</button></> }));
vi.mock("@/components/settings/sections/server-section", () => ({ ServerSection: () => <p>Server step</p> }));
vi.mock("@/components/settings/sections/memory-section", () => ({ MemorySection: () => <p>Memory step</p> }));
vi.mock("@/components/settings/sections/auth-section", () => ({ AuthSection: () => <p>Auth step</p> }));
vi.mock("@/components/settings/sections/sso-section", () => ({ SsoSection: () => <p>SSO step</p> }));
vi.mock("@/components/settings/sections/browser-section", () => ({ BrowserSection: () => <p>Browser step</p> }));
vi.mock("@/components/settings/sections/search-section", () => ({ SearchSection: () => <p>Search step</p> }));
vi.mock("@/components/settings/sections/voice-section", () => ({ VoiceSection: () => <p>Voice step</p> }));
vi.mock("@/components/settings/sections/sandbox-section", () => ({ SandboxSettingsSection: () => <p>Sandbox step</p> }));
vi.mock("@/components/settings/sections/vault-section", () => ({ ServerVaultSection: () => null }));
vi.mock("@/components/settings/sections/advanced-section", () => ({ AdvancedSection: () => null }));
vi.mock("@/components/settings/sections/users-section", () => ({ UsersSection: () => null }));
vi.mock("@/components/settings/sections/skills-section", () => ({ SkillsSection: () => null }));

const provider = { provider: "local-fixture", api_key: { is_set: false }, base_url: "http://old.invalid", enabled: true };
const brand = {
  id: "local-fixture", name: "Local fixture", adapter: "ollama", description: null, documentation_url: null, logo_url: null,
  default_base_url: "http://old.invalid", api_surfaces: ["ollama"], sources: ["recipe"], warnings: [], catalog_available: true,
  configuration_defaults: { provider: "local-fixture" }, effective_protocols: ["ollama"],
  fields: [{ id: "endpoint", label: "Endpoint", target: { kind: "configuration", path: "/base_url" }, schema: { type: "string" }, required: true, sensitive: false, suggested_env: [] }],
  auth_methods: [{ id: "anonymous", credential_method: "anonymous", access_mode: "api", priority: 0, protocols: ["ollama"], interaction: "none", persistence: "none", fields: [], validation_available: true }],
};
function document() {
  return { config: { providers: { account: provider }, models: {}, memory: { backend: "basic" }, server: { timezone: "UTC" }, auth: { encryption_secret: { is_set: true } } },
    authoring_document: { providers: { account: provider } }, persisted_revision: "persisted-before-edit", active_revision: "older-active", restart_required: true, parameter_overrides: [] };
}
beforeEach(() => {
  vi.resetAllMocks();
  window.history.replaceState(null, "", "/#providers");
  vi.mocked(api.get).mockImplementation(async path => {
    if (path === "/api/config") return document();
    if (path === "/api/config/provider-catalog") return { providers: [brand], source_status: {} };
    if (path === "/api/config/providers") return { providers: [{ handle: "account", provider: "local-fixture", configuration: provider, setup: brand,
      pending_removal: false, 
      effective_authentication: { method: "anonymous", source: "anonymous" }, credentials: [], affected_groups: [], authentication_methods: [] }] };
    return [];
  });
  vi.mocked(api.post).mockImplementation(async path => path.endsWith("/validate") ? { validation_id: "validated-page-draft", credential: { method: "anonymous", state: "pending", generation: 0, version: "draft" }, models: [] }
    : path.endsWith("/models") ? { models: [], directory_status: "live", manual_entry: true } : {});
  vi.mocked(api.put).mockResolvedValue({ ...document(), persisted_revision: "persisted-after-edit" });
});

it("uses the loaded revision, preserves the edit after a stale save, and retains restart state", async () => {
  render(<AdminSettingsPage />);
  const endpoint = await screen.findByLabelText("Endpoint *");
  expect(screen.getByText("Configuration saved. Restart the server for changes to take effect.")).toBeInTheDocument();
  fireEvent.change(endpoint, { target: { value: "http://draft.invalid" } });
  await screen.findByRole("img", { name: "Connection validated" });
  vi.mocked(api.put).mockRejectedValueOnce(new Error("stale persisted revision"));
  await waitFor(() => expect(screen.getByRole("button", { name: /^Save$/ })).toBeEnabled());
  fireEvent.click(screen.getByRole("button", { name: /^Save$/ }));
  await screen.findByText("stale persisted revision");
  expect(endpoint).toHaveValue("http://draft.invalid");
  expect(api.put).toHaveBeenCalledWith("/api/config", expect.objectContaining({ expected_persisted_revision: "persisted-before-edit",
    patch: expect.objectContaining({ providers: expect.objectContaining({ account: expect.objectContaining({ base_url: "http://draft.invalid" }) }) }) }));
  expect(api.get).toHaveBeenCalledWith("/api/config");
});

it.each(["settings", "setup"])("saves a validated API key in %s and retains its reference after a failed save", async page => {
  const keyBrand = { ...brand, auth_methods: [{ ...brand.auth_methods[0], id: "api-key", credential_method: "api_key", interaction: "form", persistence: "database",
    fields: [{ id: "key", label: "API key", target: { kind: "credential", field: "api_key" }, schema: { type: "string" }, required: true, sensitive: true, suggested_env: [] }] }] };
  const original = vi.mocked(api.get).getMockImplementation()!;
  vi.mocked(api.get).mockImplementation(async (path, ...args) => {
    if (path === "/api/config/provider-catalog") return { providers: [keyBrand], source_status: {} };
    if (path === "/api/config/providers") return { providers: [{ handle: "account", configuration: provider, setup: keyBrand, credentials: [], affected_groups: [], authentication_methods: [] }] };
    return original(path, ...args);
  });
  vi.mocked(api.post).mockImplementation(async path => path.endsWith("/validate")
    ? { validation_id: "key-proof", credential: { method: "api_key", state: "pending", generation: 0, version: "draft" }, models: [] }
    : path.endsWith("/credentials") ? { credential_id: "saved-key" }
    : path.endsWith("/models") ? { models: [], directory_status: "live", manual_entry: true } : {});
  render(page === "settings" ? <AdminSettingsPage /> : <SetupPage />);
  if (page === "setup") {
    fireEvent.click(await screen.findByRole("button", { name: "Next" }));
    fireEvent.click(screen.getByRole("button", { name: "Next" }));
  }
  fireEvent.change(await screen.findByLabelText("API key *"), { target: { value: "test-key" } });
  await screen.findByRole("img", { name: "Connection validated" }, { timeout: 3000 });
  await waitFor(() => expect(screen.getByRole("button", { name: page === "settings" ? /^Save$/ : "Next" })).toBeEnabled());
  expect(screen.getByLabelText("API key *")).toHaveAttribute("placeholder", "\u2022".repeat(8));
  expect(api.post).not.toHaveBeenCalledWith(expect.stringMatching(/\/credentials$/), expect.anything());
  if (page === "setup") {
    while (screen.queryByRole("button", { name: "Next" })) fireEvent.click(screen.getByRole("button", { name: "Next" }));
  }
  const saveButton = page === "settings" ? /^Save$/ : "Complete Setup";
  vi.mocked(api.put).mockRejectedValueOnce(new Error("stale persisted revision"));
  fireEvent.click(screen.getByRole("button", { name: saveButton }));
  await screen.findByText("stale persisted revision");
  expect(api.put).toHaveBeenLastCalledWith("/api/config", expect.objectContaining({ patch: expect.objectContaining({ providers: expect.objectContaining({ account: expect.objectContaining({ credential_id: "saved-key", api_key: null }) }) }) }));
  expect(JSON.stringify(vi.mocked(api.put).mock.calls)).not.toContain("test-key");
  fireEvent.click(screen.getByRole("button", { name: saveButton }));
  await waitFor(() => expect(api.put).toHaveBeenCalledTimes(2));
  expect(vi.mocked(api.post).mock.calls.filter(([path]) => path.endsWith("/credentials"))).toHaveLength(1);
});

it("keeps external validation through setup-step unmounts without sending proofs to config saves", async () => {
  render(<SetupPage />);
  await screen.findByRole("button", { name: "Next" });
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  const endpoint = await screen.findByLabelText("Endpoint *");
  fireEvent.change(endpoint, { target: { value: "http://setup-draft.invalid" } });
  await waitFor(() => expect(screen.getByRole("button", { name: "Next" })).toBeEnabled());
  while (screen.queryByRole("button", { name: "Next" })) fireEvent.click(screen.getByRole("button", { name: "Next" }));
  fireEvent.click(screen.getByRole("button", { name: "Complete Setup" }));
  await screen.findByText("Setup Complete");
  expect(api.put).toHaveBeenCalledWith("/api/config", expect.objectContaining({ expected_persisted_revision: "persisted-before-edit" }));
});

it("discarding an in-flight validation cannot restore the abandoned endpoint or proof", async () => {
  let finish: (value: unknown) => void = () => {};
  const pending = new Promise(resolve => { finish = resolve; });
  vi.mocked(api.post).mockImplementation(async path => path.endsWith("/validate") ? pending : {});
  render(<AdminSettingsPage />);
  fireEvent.change(await screen.findByLabelText("Endpoint *"), { target: { value: "http://abandoned.invalid" } });
  await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/account/validate", expect.anything()));
  fireEvent.click(screen.getByRole("button", { name: "Discard" }));
  await waitFor(() => expect(screen.getByLabelText("Endpoint *")).toHaveValue("http://old.invalid"));
  await act(async () => finish({ validation_id: "abandoned-proof", credential: { method: "anonymous", state: "pending", generation: 0, version: "draft" }, models: [] }));
  await waitFor(() => expect(api.delete).toHaveBeenCalledWith("/api/config/providers/account/drafts/abandoned-proof"));
  expect(screen.getByLabelText("Endpoint *")).toHaveValue("http://old.invalid");
  expect(screen.queryByRole("img", { name: "Connection validated" })).not.toBeInTheDocument();
  expect(api.put).not.toHaveBeenCalled();
});
