import React, { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { LoginChallenge, ProvidersSection } from "../providers-section";
import { api } from "@/lib/api-client";
import { updateConfig, type ModelProviderConfig } from "@/lib/config-types";
import type { LoginAttempt, ProviderCatalog, ProviderCatalogEntry, ProviderInspection } from "@/lib/provider-admin";
import { acceptProviderDrafts, type ProviderDrafts } from "@/lib/provider-drafts";

vi.mock("@/lib/api-client", () => ({ api: { get: vi.fn(), post: vi.fn(), put: vi.fn(), delete: vi.fn() } }));

const brand: ProviderCatalogEntry = {
  id: "nova", name: "Unseen Nova", description: null, documentation_url: null, logo_url: null,
  adapter: "openai", default_base_url: "https://nova.invalid/v1", api_surfaces: ["completions"],
  fields: [
    { id: "base_url", target: { kind: "configuration", path: "/base_url" }, label: "Endpoint", required: false, sensitive: false, schema: { type: "string" }, suggested_env: [] },
    { id: "enabled", target: { kind: "configuration", path: "/enabled" }, label: "Enabled", required: false, sensitive: false, schema: { type: "boolean" }, suggested_env: [] },
  ],
  auth_methods: [{ id: "api-key", credential_method: "api_key", access_mode: "api", priority: 0, protocols: ["completions"],
    interaction: "form", persistence: "database", validation_available: true,
    fields: [{ id: "secret", target: { kind: "credential", field: "api_key" }, label: "Nova secret", required: true, sensitive: true, schema: { type: "string" }, suggested_env: [] }] }],
  configuration_defaults: { provider: "nova", adapter: "openai", base_url: "https://nova.invalid/v1" },
  sources: ["models.dev"], warnings: [], catalog_available: true, effective_protocols: null,
};
let catalog: ProviderCatalog;
let inspections: ProviderInspection[];
const config: ModelProviderConfig = { provider: "nova", adapter: "openai", api_key: { is_set: true }, base_url: "https://nova.invalid/v1", enabled: true };
function inspection(overrides: Partial<ProviderInspection> = {}): ProviderInspection {
  return { handle: "account", provider: "nova", adapter: "openai", configuration: config, setup: brand,
    pending_removal: false, 
    effective_authentication: { method: "api_key", source: "database" }, authentication_methods: [],
    credentials: [{ method: "api_key", state: "active", generation: 3, version: "active" }], affected_groups: ["primary"], ...overrides };
}
function Harness({ initial = {} }: { initial?: Record<string, ModelProviderConfig> }) {
  const [providers, setProviders] = useState(initial);
  const [drafts, setDrafts] = useState<ProviderDrafts>({});
  const [block, setBlock] = useState<string | null>(null);
  const [revision, setRevision] = useState("revision-1");
  const [notice, setNotice] = useState("");
  const [dirty, setDirty] = useState(false);
  return <><ProvidersSection providers={providers} onChange={next => { setProviders(next); setDirty(true); }}
    drafts={drafts} onDraftsChange={setDrafts} persistedRevision={revision} hasUnsavedChanges={dirty} onReadyChange={setBlock}
    onSaved={result => { setProviders(result.config.providers); setRevision(result.persisted_revision); setDirty(false); }} />
    <button disabled={!!block} onClick={async () => {
      try { const patch = await acceptProviderDrafts({ providers }, drafts, (handle, connection) => {
        setProviders(previous => ({ ...previous, [handle]: connection }));
        setDrafts(previous => { const next = { ...previous }; delete next[handle]; return next; });
      });
        const result = await updateConfig(patch, { expectedPersistedRevision: revision });
        setRevision(result.persisted_revision); setNotice(result.restart_required ? "Restart required" : "Saved"); }
      catch (error) { setNotice(error instanceof Error ? error.message : "Failed"); }
    }}>Save fixture</button><p>{notice}</p><output data-testid="draft-state">{JSON.stringify({ providers, drafts })}</output></>;
}
async function add() {
  fireEvent.change(await screen.findByRole("combobox", { name: "Provider brand" }), { target: { value: "Unseen" } });
  expect(screen.getByRole("button", { name: "Add connection" })).toBeDisabled();
  fireEvent.click(await screen.findByRole("option", { name: "Unseen Nova" }));
  fireEvent.click(screen.getByRole("button", { name: "Add connection" }));
  return screen.getByRole("region", { name: "Connection nova" });
}
async function chooseOption(card: HTMLElement, label: string, option: string) {
  const input = await within(card).findByRole("combobox", { name: label });
  fireEvent.keyDown(input, { key: "ArrowDown" });
  fireEvent.click(await within(card).findByRole("option", { name: option }));
}
async function validate(card: HTMLElement, key = "secret-fixture") {
  fireEvent.change(within(card).getByLabelText("Nova secret *"), { target: { value: key } });
  await waitFor(() => expect(within(card).getByRole("img", { name: /Connection validat(ed|ion failed)/ })).toBeInTheDocument(), { timeout: 3000 });
}

beforeEach(() => {
  vi.resetAllMocks();
  catalog = { providers: [structuredClone(brand)], source_status: {} }; inspections = [];
  vi.spyOn(window, "confirm").mockReturnValue(true);
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config/provider-catalog" ? catalog
    : path === "/api/config/provider-credentials" ? []
    : path === "/api/config/environment-variables" ? ["SERVER_NOVA_KEY"]
    : { providers: inspections });
  vi.mocked(api.post).mockImplementation(async path => path.endsWith("/validate") ? {
    validation_id: "proof-1", credential: { method: "api_key", state: "pending", generation: 0, version: "draft", validation_id: "proof-1" }, models: [],
  } : path.endsWith("/credentials") ? {credential_id: "saved-credential", method: "api_key", state: "active", version: "accepted", generation: 1} : path.endsWith("/models") ? { models: [], directory_status: "live", manual_entry: true } : {});
  vi.mocked(api.put).mockResolvedValue({ config: { providers: {} }, persisted_revision: "revision-2", restart_required: true });
  vi.mocked(api.delete).mockResolvedValue({ discarded: true });
});

describe("metadata-driven provider connections", () => {
  it.each<NonNullable<LoginAttempt["challenge"]>["kind"]>(["device_code", "redirect"])("copies only the code from a %s login challenge", async kind => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    render(<LoginChallenge attempt={{ id: "copy-attempt", status: "pending", credential_id: null,
      challenge: { kind, url: "https://login.invalid", user_code: "ABCD-1234", message: "Login instructions" } }} />);
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledWith("ABCD-1234");
    expect(await screen.findByRole("button", { name: "Copied" })).toBeInTheDocument();
  });

  it.each(["anonymous", "environment"])("allows renaming after automatic %s validation and validates the new handle", async source => {
    if (source === "anonymous") {
      catalog.providers[0].auth_methods = [{ ...brand.auth_methods[0], id: "anonymous", credential_method: "anonymous", interaction: "none", fields: [] }];
    } else {
      catalog.providers[0].auth_methods[0].fields[0].suggested_env = ["SERVER_NOVA_KEY"];
    }
    render(<Harness />);
    const card = await add();
    await within(card).findByRole("img", { name: "Connection validated" }, { timeout: 3000 });
    const handle = within(card).getByLabelText("Connection handle");
    expect(handle).toBeEnabled();
    expect(within(card).queryByText(/The handle is locked/)).not.toBeInTheDocument();
    fireEvent.change(handle, { target: { value: "renamed" } });
    fireEvent.blur(handle);
    const renamed = screen.getByRole("region", { name: "Connection renamed" });
    expect(JSON.parse(screen.getByTestId("draft-state").textContent!).drafts.nova).toBeUndefined();
    await waitFor(() => expect(api.delete).toHaveBeenCalledWith("/api/config/providers/nova/drafts/proof-1"));
    await within(renamed).findByRole("img", { name: "Connection validated" }, { timeout: 3000 });
    expect(api.post).toHaveBeenCalledWith("/api/config/providers/renamed/validate", expect.objectContaining({
      credential: source === "anonymous" ? { source: "anonymous" } : { source: "environment", variable: "SERVER_NOVA_KEY" },
    }));
    expect(within(renamed).getByLabelText("Connection handle")).toBeEnabled();
  });

  it("debounces automatic validation and shows progress beside the provider name", async () => {
    let finishValidation!: (result: unknown) => void;
    const pending = new Promise(resolve => { finishValidation = resolve; });
    vi.mocked(api.post).mockImplementation(async path => path.endsWith("/validate") ? pending
      : path.endsWith("/models") ? { models: [], directory_status: "live", manual_entry: true } : {});
    render(<Harness />);
    const card = await add();
    expect(within(card).queryByRole("button", { name: "Validate draft" })).not.toBeInTheDocument();
    vi.useFakeTimers();
    try {
      await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
      expect(api.post).not.toHaveBeenCalled();
      const input = within(card).getByLabelText("Nova secret *");
      fireEvent.change(input, { target: { value: "unfinished" } });
      await act(async () => { await vi.advanceTimersByTimeAsync(700); });
      fireEvent.change(input, { target: { value: "complete-key" } });
      await act(async () => { await vi.advanceTimersByTimeAsync(799); });
      expect(api.post).not.toHaveBeenCalled();
      await act(async () => { await vi.advanceTimersByTimeAsync(1); });
      expect(api.post).toHaveBeenCalledWith("/api/config/providers/nova/validate", expect.objectContaining({
        credential: { source: "api_key", api_key: "complete-key" },
      }));
      expect(within(within(card).getByRole("heading")).getByRole("img", { name: "Validating connection" })).toBeInTheDocument();
      await act(async () => { finishValidation({ validation_id: "auto-proof", credential: { method: "api_key", state: "pending", generation: 0, version: "draft" }, models: [] }); });
      expect(within(card).getByRole("img", { name: "Connection validated" })).toBeInTheDocument();
      await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
      expect(vi.mocked(api.post).mock.calls.filter(([path]) => path.endsWith("/validate"))).toHaveLength(1);
    } finally { vi.useRealTimers(); }
  });

  it.each([
    { name: "empty", records: [] },
    { name: "incompatible", records: [{ credential_id: "foreign", integration: "openai_codex", name: "ChatGPT" }] },
  ])("hides saved authentication for $name results without flashing the lock notice", async ({ records }) => {
    let finishLoading!: (records: unknown[]) => void;
    const request = new Promise<unknown[]>(resolve => { finishLoading = resolve; });
    vi.mocked(api.get).mockImplementation(async path => path === "/api/config/provider-catalog" ? catalog
      : path === "/api/config/provider-credentials" ? request
      : path === "/api/config/environment-variables" ? ["SERVER_NOVA_KEY"] : { providers: inspections });
    render(<Harness />);
    const card = await add();
    const handle = within(card).getByLabelText("Connection handle");
    const authentication = within(card).getByRole("combobox", { name: "Authentication method" });
    fireEvent.keyDown(authentication, { key: "ArrowDown" });
    expect(within(card).queryByRole("option", { name: "Saved credential" })).not.toBeInTheDocument();
    expect(within(card).queryByText("The handle is locked after saving or starting database authentication.")).not.toBeInTheDocument();
    await act(async () => { finishLoading(records); });
    expect(within(card).queryByRole("option", { name: "Saved credential" })).not.toBeInTheDocument();
    expect(within(card).queryByRole("combobox", { name: "Saved credential" })).not.toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Choose saved credential" })).not.toBeInTheDocument();
    expect(within(card).getByRole("combobox", { name: "Authentication method" })).toBe(authentication);
    expect(within(card).getByLabelText("Connection handle")).toBe(handle);
    expect(handle).toBeEnabled();
  });

  it("offers saved authentication after retrying a failed credential lookup", async () => {
    let credentialRequests = 0;
    vi.mocked(api.get).mockImplementation(async path => {
      if (path === "/api/config/provider-catalog") return catalog;
      if (path === "/api/config/environment-variables") return ["SERVER_NOVA_KEY"];
      if (path === "/api/config/provider-credentials") {
        if (++credentialRequests === 1) throw new Error("Could not load saved credentials");
        return [{ credential_id: "saved-id", integration: "static", name: "Existing account" }];
      }
      return { providers: inspections };
    });
    render(<Harness />);
    const card = await add();
    await within(card).findByText("Could not load saved credentials");
    expect(within(card).queryByRole("combobox", { name: "Saved credential" })).not.toBeInTheDocument();
    fireEvent.click(within(card).getByRole("button", { name: "Retry saved credentials" }));
    await chooseOption(card, "Authentication method", "Saved credential");
    expect(within(card).queryByText("Could not load saved credentials")).not.toBeInTheDocument();
    await chooseOption(card, "Saved credential", "Existing account");
    expect(within(card).getByRole("combobox", { name: "Saved credential" })).toHaveValue("Existing account");
  });

  it("loads metadata before fields, normalizes handles, and submits secrets only for validation", async () => {
    render(<Harness />);
    expect(screen.queryByLabelText("Nova secret *")).not.toBeInTheDocument();
    const first = await add();
    expect(first.querySelector("select")).toBeNull();
    expect(screen.getByRole("combobox", { name: "Provider brand" })).toHaveValue("Unseen Nova");
    expect(within(first).getByRole("combobox", { name: "Authentication method" })).toHaveValue("API key");
    const handle = within(first).getByLabelText("Connection handle");
    fireEvent.change(handle, { target: { value: " Work_2 " } }); fireEvent.blur(handle);
    const card = screen.getByRole("region", { name: "Connection work_2" });
    await validate(card);
    expect(within(card).getByLabelText("Connection handle")).toBeEnabled();
    expect(within(card).getByLabelText("Nova secret *")).toHaveValue("");
    expect(screen.getByTestId("draft-state")).not.toHaveTextContent("secret-fixture");
    expect(api.post).toHaveBeenCalledWith("/api/config/providers/work_2/validate", { config: { provider: "nova", adapter: "openai", base_url: "https://nova.invalid/v1", api_key: null, enabled: true }, credential: { source: "api_key", api_key: "secret-fixture" } });
    const listBody = vi.mocked(api.post).mock.calls.find(([path]) => path.endsWith("/models"))?.[1];
    expect(listBody).not.toHaveProperty("fingerprint"); expect(listBody).not.toHaveProperty("expiresAt");
    expect(api.get).not.toHaveBeenCalledWith(expect.stringContaining("secret-fixture"));
    await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "Save fixture" }));
    await screen.findByText("Restart required");
    expect(api.put).toHaveBeenCalledWith("/api/config", expect.objectContaining({ expected_persisted_revision: "revision-1", patch: expect.objectContaining({providers: expect.objectContaining({work_2: expect.objectContaining({credential_id: "saved-credential"})})}) }));
  });

  it("keeps two connections for an unseen brand and invalidates a proof after endpoint changes", async () => {
    render(<Harness />); const first = await add(); await validate(first);
    fireEvent.click(screen.getByRole("button", { name: "Add connection" }));
    const second = screen.getByRole("region", { name: "Connection nova-2" }); await validate(second, "second-secret");
    expect(screen.getByTestId("draft-state")).toHaveTextContent('"nova-2"');
    fireEvent.change(within(first).getByLabelText("Endpoint"), { target: { value: "https://different.invalid/v1" } });
    await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeDisabled());
    expect(JSON.parse(screen.getByTestId("draft-state").textContent!).drafts.nova).toBeUndefined();
    expect(JSON.parse(screen.getByTestId("draft-state").textContent!).drafts["nova-2"]).toBeDefined();
  });

  it("blocks failed validation, clears its secret, and retains accepted credentials after a stale save", async () => {
    vi.mocked(api.post).mockRejectedValueOnce(new Error("credential rejected"));
    render(<Harness />); const card = await add(); await validate(card);
    expect(within(card).getByRole("alert")).toHaveTextContent("credential rejected");
    expect(screen.getByRole("button", { name: "Save fixture" })).toBeDisabled();
    expect(within(card).getByLabelText("Nova secret *")).toHaveValue("");
    await validate(card, "retry-secret");
    vi.mocked(api.put).mockRejectedValueOnce(new Error("configuration revision conflict"));
    await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "Save fixture" }));
    await screen.findByText("configuration revision conflict");
    expect(screen.getByTestId("draft-state")).toHaveTextContent("saved-credential");
    expect(screen.getByTestId("draft-state")).not.toHaveTextContent("retry-secret");
  });

  it("renders generic backend login challenges without provider-name branches", async () => {
    catalog.providers[0].auth_methods = [{ ...brand.auth_methods[0], id: "device", credential_method: "oauth", interaction: "backend_login", fields: [] }];
    vi.mocked(api.post).mockResolvedValue({ id: "attempt-1", status: "pending", challenge: { kind: "device_code", url: "https://login.invalid/device", user_code: "ABCD", message: "Use this code" } });
    render(<Harness />); const card = await add();
    expect(within(card).queryByLabelText("Nova secret *")).not.toBeInTheDocument();
    vi.useFakeTimers();
    try {
      await act(async () => { fireEvent.click(within(card).getByRole("button", { name: "Connect" })); });
      expect(screen.getByText("ABCD")).toBeInTheDocument();
      expect(within(card).getByLabelText("Connection handle")).toBeEnabled();
      expect(screen.getByRole("link", { name: "Open provider login" })).toHaveAttribute("href", "https://login.invalid/device");
      expect(api.post).toHaveBeenCalledWith("/api/config/providers/nova/login/start", expect.objectContaining({ method: "oauth" }));
      vi.mocked(api.get).mockResolvedValueOnce({ id: "attempt-1", status: "validated", challenge: null, credential_id: "oauth-credential" });
      vi.mocked(api.post).mockResolvedValue({ models: [], directory_status: "live" });
      expect(within(card).queryByRole("button", { name: "Check login status" })).not.toBeInTheDocument();
      await act(async () => { await vi.advanceTimersByTimeAsync(9999); });
      expect(api.get).not.toHaveBeenCalledWith("/api/config/providers/nova/login/attempt-1");
      await act(async () => { await vi.advanceTimersByTimeAsync(1); });
      expect(screen.getByTestId("draft-state")).toHaveTextContent("oauth-credential");
      expect(api.get).toHaveBeenCalledWith("/api/config/providers/nova/login/attempt-1");
      expect(api.post).not.toHaveBeenCalledWith("/api/config/providers/nova/validate", expect.anything());
    } finally { vi.useRealTimers(); }
  });

  it.each(["validated", "failed", "expired", "cancelled"])("keeps polling pending logins and stops after %s", async status => {
    catalog.providers[0].auth_methods = [{ ...brand.auth_methods[0], id: "device", credential_method: "oauth", interaction: "backend_login", fields: [] }];
    const pending = { id: "poll-attempt", status: "pending", challenge: { kind: "device_code", url: "https://login.invalid", user_code: "CODE" }, credential_id: null };
    vi.mocked(api.post).mockResolvedValue(pending);
    const originalGet = vi.mocked(api.get).getMockImplementation()!;
    let polls = 0;
    vi.mocked(api.get).mockImplementation(async (path, ...args) => {
      if (path !== "/api/config/providers/nova/login/poll-attempt") return originalGet(path, ...args);
      polls += 1;
      if (polls === 1) throw new Error("Temporary connection failure");
      return polls === 2 ? pending : { ...pending, status, challenge: null, credential_id: status === "validated" ? "poll-credential" : null };
    });
    render(<Harness />);
    const card = await add();
    vi.useFakeTimers();
    try {
      await act(async () => { fireEvent.click(within(card).getByRole("button", { name: "Connect" })); });
      await act(async () => { await vi.advanceTimersByTimeAsync(10000); });
      expect(polls).toBe(1);
      expect(within(card).getByRole("button", { name: "Cancel login" })).toBeEnabled();
      await act(async () => { await vi.advanceTimersByTimeAsync(10000); });
      expect(polls).toBe(2);
      expect(within(card).getByRole("img", { name: "Validating connection" })).toBeInTheDocument();
      expect(within(card).queryByText("Temporary connection failure")).not.toBeInTheDocument();
      await act(async () => { await vi.advanceTimersByTimeAsync(10000); });
      expect(polls).toBe(3);
      expect(within(card).queryByRole("button", { name: "Cancel login" })).not.toBeInTheDocument();
      await act(async () => { await vi.advanceTimersByTimeAsync(50000); });
      expect(polls).toBe(3);
      if (status === "validated") expect(screen.getByTestId("draft-state")).toHaveTextContent("poll-credential");
    } finally { vi.useRealTimers(); }
  });

  it("stops polling and ignores an in-flight login response when the card is removed", async () => {
    catalog.providers[0].auth_methods = [{ ...brand.auth_methods[0], id: "device", credential_method: "oauth", interaction: "backend_login", fields: [] }];
    vi.mocked(api.post).mockResolvedValue({ id: "removed-attempt", status: "pending", challenge: { kind: "device_code", url: "https://login.invalid", user_code: "CODE" } });
    let finishPolling!: (result: unknown) => void;
    const request = new Promise(resolve => { finishPolling = resolve; });
    const originalGet = vi.mocked(api.get).getMockImplementation()!;
    let polls = 0;
    vi.mocked(api.get).mockImplementation(async (path, ...args) => {
      if (path !== "/api/config/providers/nova/login/removed-attempt") return originalGet(path, ...args);
      polls += 1;
      return request;
    });
    render(<Harness />);
    const card = await add();
    vi.useFakeTimers();
    try {
      await act(async () => { fireEvent.click(within(card).getByRole("button", { name: "Connect" })); });
      await act(async () => { await vi.advanceTimersByTimeAsync(40000); });
      expect(polls).toBe(1);
      const remove = within(card).getByRole("button", { name: "Remove draft" });
      expect(remove).toBeEnabled();
      await act(async () => { fireEvent.click(remove); });
      expect(api.delete).toHaveBeenCalledWith("/api/config/providers/nova/login/removed-attempt");
      await act(async () => { finishPolling({ id: "removed-attempt", status: "validated", challenge: null, credential_id: "abandoned-credential" }); });
      await act(async () => { await vi.advanceTimersByTimeAsync(50000); });
      expect(polls).toBe(1);
      expect(screen.queryByRole("region", { name: "Connection nova" })).not.toBeInTheDocument();
      expect(screen.getByTestId("draft-state")).not.toHaveTextContent("abandoned-credential");
    } finally { vi.useRealTimers(); }
  });

  it("connects an API-key account and clears the previous environment source", async () => {
    catalog.providers[0].auth_methods.push({ ...brand.auth_methods[0], id: "openrouter_connect", interaction: "backend_login", fields: [] });
    render(<Harness />); const card = await add();
    await chooseOption(card, "Credential source", "Server environment variable");
    fireEvent.change(within(card).getByRole("combobox", { name: "Environment variable" }), { target: { value: "OLD_KEY" } });
    await chooseOption(card, "Authentication method", "Connect OpenRouter");
    vi.mocked(api.post).mockResolvedValueOnce({ id: "connect-1", status: "pending", challenge: { kind: "redirect", url: "https://openrouter.ai/auth" } });
    fireEvent.click(within(card).getByRole("button", { name: "Connect" }));
    const code = await within(card).findByLabelText("Login completion code");
    expect(api.post).toHaveBeenCalledWith("/api/config/providers/nova/login/start", expect.objectContaining({ method: "api_key", config: expect.objectContaining({ api_key: null }) }));
    fireEvent.change(code, { target: { value: "private-code" } });
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    fireEvent.click(within(card).getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledWith("private-code");
    vi.mocked(api.post).mockResolvedValueOnce({ id: "connect-1", status: "validated", challenge: null, credential_id: "connected-credential" });
    fireEvent.click(within(card).getByRole("button", { name: "Complete login" }));
    await waitFor(() => expect(screen.getByTestId("draft-state")).toHaveTextContent("connected-credential"));
    expect(screen.getByTestId("draft-state")).not.toHaveTextContent("private-code");
    expect(screen.getByTestId("draft-state")).not.toHaveTextContent("OLD_KEY");
    expect(within(card).queryByLabelText("Login completion code")).not.toBeInTheDocument();
  });

  it("submits redirect completion through the generic login endpoint and clears the code", async () => {
    catalog.providers[0].auth_methods = [{ ...brand.auth_methods[0], id: "redirect", credential_method: "oauth", interaction: "backend_login", fields: [] }];
    vi.mocked(api.post).mockResolvedValueOnce({ id: "redirect-1", status: "pending", challenge: { kind: "redirect", url: "https://login.invalid/authorize" } });
    render(<Harness />); const card = await add();
    fireEvent.click(within(card).getByRole("button", { name: "Connect" }));
    const code = await within(card).findByLabelText("Login completion code");
    expect(code).toHaveAttribute("type", "password");
    fireEvent.change(code, { target: { value: "private-completion-code" } });
    vi.mocked(api.post).mockResolvedValueOnce({ id: "redirect-1", status: "failed", challenge: null, error: "Login denied" });
    fireEvent.click(within(card).getByRole("button", { name: "Complete login" }));
    await screen.findByText("Login denied");
    expect(api.post).toHaveBeenLastCalledWith("/api/config/providers/nova/login/redirect-1/complete", { code: "private-completion-code" });
    expect(within(card).queryByLabelText("Login completion code")).not.toBeInTheDocument();
  });

  it("keeps existing removed-brand forms, active replacement errors, logout warnings, and referenced deletion visible", async () => {
    catalog.providers = []; inspections = [inspection()];
    render(<Harness initial={{ account: config }} />);
    const card = await screen.findByRole("region", { name: "Connection account" });
    expect(within(card).getByLabelText("Connection handle")).toBeEnabled();
    expect(within(card).getByText("Used by: primary")).toBeInTheDocument();
    vi.mocked(api.delete).mockRejectedValueOnce(new Error("provider is referenced by primary"));
    fireEvent.click(within(card).getByRole("button", { name: "Delete connection" }));
    await screen.findByText("provider is referenced by primary");
    await chooseOption(card, "Credential source", "Enter a new API key");
    await validate(card);
    vi.mocked(api.post).mockRejectedValueOnce(new Error("active binding changed"));
    fireEvent.click(screen.getByRole("button", { name: "Save fixture" }));
    await screen.findByText("active binding changed");
    expect(api.post).toHaveBeenLastCalledWith("/api/config/providers/account/credentials", expect.objectContaining({validation_id: "proof-1"}));
    vi.mocked(api.delete).mockResolvedValueOnce({ affected_groups: ["primary"], unavailable_models: { primary: [["model", "unsupported_protocol_for_auth_method"]] } });
    fireEvent.click(within(card).getByRole("button", { name: "Log out API key" }));
    await screen.findByText(/This authentication method does not support the selected protocol/);
    fireEvent.click(within(card).getByLabelText("Enabled"));
    expect(JSON.parse(screen.getByTestId("draft-state").textContent!).providers.account.enabled).toBe(false);
  });

  it("shows an existing compatible credential as the selected authentication method", async () => {
    const savedConfig = { ...config, api_key: null, credential_id: "saved-id" };
    inspections = [inspection({ configuration: savedConfig })];
    vi.mocked(api.get).mockImplementation(async path => path === "/api/config/provider-catalog" ? catalog
      : path === "/api/config/provider-credentials" ? [{ credential_id: "saved-id", integration: "static", name: "Existing account" }]
      : path === "/api/config/environment-variables" ? ["SERVER_NOVA_KEY"]
      : { providers: inspections });
    render(<Harness initial={{ account: savedConfig }} />);
    const card = await screen.findByRole("region", { name: "Connection account" });
    expect(await within(card).findByRole("combobox", { name: "Saved credential" })).toHaveValue("Existing account");
    expect(within(card).getByRole("combobox", { name: "Authentication method" })).toHaveValue("Saved credential");
    expect(within(card).queryByRole("combobox", { name: "Credential source" })).not.toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Validate draft" })).not.toBeInTheDocument();
    expect(api.post).not.toHaveBeenCalled();
  });

  it("selects saved credentials without repeating login or validation", async () => {
    vi.mocked(api.get).mockImplementation(async path => path === "/api/config/provider-catalog" ? catalog
      : path === "/api/config/provider-credentials" ? [
        {credential_id: "saved-id", integration: "static", name: "Existing account"},
        {credential_id: "incompatible-id", integration: "openai_codex", name: "ChatGPT"}
      ] : path === "/api/config/environment-variables" ? ["SERVER_NOVA_KEY"] : {providers: inspections});
    render(<Harness />);
    const card = await add();
    await chooseOption(card, "Authentication method", "Saved credential");
    expect(within(card).queryByLabelText("Nova secret *")).not.toBeInTheDocument();
    expect(within(card).queryByRole("combobox", { name: "Credential source" })).not.toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Validate draft" })).not.toBeInTheDocument();
    const picker = within(card).getByRole("combobox", { name: "Saved credential" });
    fireEvent.keyDown(picker, { key: "ArrowDown" });
    expect(within(card).queryByRole("option", { name: "ChatGPT" })).not.toBeInTheDocument();
    fireEvent.click(within(card).getByRole("option", { name: "Existing account" }));
    expect(within(card).queryByRole("option", { name: "ChatGPT" })).not.toBeInTheDocument();
    expect(within(card).getByRole("combobox", { name: "Saved credential" })).toHaveValue("Existing account");
    await waitFor(() => expect(screen.getByRole("button", {name: "Save fixture"})).toBeEnabled());
    expect(screen.getByTestId("draft-state")).toHaveTextContent("saved-id");
    expect(api.post).not.toHaveBeenCalled();
    await chooseOption(card, "Authentication method", "API key");
    expect(within(card).queryByRole("combobox", { name: "Saved credential" })).not.toBeInTheDocument();
    expect(within(card).getByLabelText("Nova secret *")).toBeInTheDocument();
    expect(JSON.parse(screen.getByTestId("draft-state").textContent!).providers.nova.credential_id).toBeNull();
    expect(screen.getByRole("button", { name: "Save fixture" })).toBeDisabled();
  });

  it("automatically selects an available suggested environment variable without showing the note", async () => {
    catalog.providers[0].auth_methods[0].fields[0].suggested_env = ["MISSING_NOVA_KEY", "SERVER_NOVA_KEY"];
    render(<Harness />);
    const card = await add();
    const input = await within(card).findByRole("combobox", { name: "Environment variable" });
    expect(input).toHaveValue("SERVER_NOVA_KEY");
    expect(within(card).getByRole("combobox", { name: "Credential source" })).toHaveValue("Server environment variable");
    expect(within(card).queryByText(/Environment variables:/)).not.toBeInTheDocument();
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/nova/validate", expect.objectContaining({
      credential: { source: "environment", variable: "SERVER_NOVA_KEY" },
    })));
    await waitFor(() => expect(within(card).getByRole("img", { name: /Connection validat(ed|ion failed)/ })).toBeInTheDocument(), { timeout: 3000 });
    await chooseOption(card, "Credential source", "Enter a new API key");
    expect(within(card).getByLabelText("Nova secret *")).toBeInTheDocument();
    expect(within(card).queryByText(/Environment variables:/)).not.toBeInTheDocument();
  });

  it("preserves a typed API key when environment suggestions arrive later", async () => {
    catalog.providers[0].auth_methods[0].fields[0].suggested_env = ["SERVER_NOVA_KEY"];
    let finishLoading!: (names: string[]) => void;
    const request = new Promise<string[]>(resolve => { finishLoading = resolve; });
    vi.mocked(api.get).mockImplementation(async path => path === "/api/config/provider-catalog" ? catalog
      : path === "/api/config/provider-credentials" ? []
      : path === "/api/config/environment-variables" ? request : { providers: inspections });
    render(<Harness />);
    const card = await add();
    fireEvent.change(within(card).getByLabelText("Nova secret *"), { target: { value: "typed-key" } });
    await act(async () => { finishLoading(["SERVER_NOVA_KEY"]); });
    expect(within(card).getByRole("combobox", { name: "Credential source" })).toHaveValue("Enter a new API key");
    expect(within(card).getByLabelText("Nova secret *")).toHaveValue("typed-key");
  });

  it("loads server environment names and accepts custom names", async () => {
    catalog.providers[0].auth_methods[0].fields[0].suggested_env = ["CATALOG_HINT_NOT_ON_SERVER"];
    render(<Harness />);
    const card = await add();
    await chooseOption(card, "Credential source", "Server environment variable");
    const input = within(card).getByRole("combobox", { name: "Environment variable" });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const serverOption = await within(card).findByRole("option", { name: "SERVER_NOVA_KEY" });
    expect(api.get).toHaveBeenCalledWith("/api/config/environment-variables");
    expect(within(card).queryByRole("option", { name: "CATALOG_HINT_NOT_ON_SERVER" })).not.toBeInTheDocument();
    fireEvent.click(serverOption);
    expect(input).toHaveValue("SERVER_NOVA_KEY");
    for (const variable of ["SERVER_NOVA_KEY", "CUSTOM_NOVA_KEY"]) {
      if (variable === "CUSTOM_NOVA_KEY") {
        fireEvent.change(input, { target: { value: variable } });
        fireEvent.blur(input);
      }
      await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/nova/validate", expect.objectContaining({
        config: expect.objectContaining({ api_key: "${" + variable + "}" }),
        credential: { source: "environment", variable },
      })));
      await waitFor(() => expect(within(card).getByRole("img", { name: /Connection validat(ed|ion failed)/ })).toBeInTheDocument(), { timeout: 3000 });
      expect(input).toHaveValue(variable);
    }
  });

  it("keeps environment references external and renders ambient selectors from metadata", async () => {
    catalog.providers[0].auth_methods[0].fields[0].suggested_env = ["SERVER_NOVA_KEY"];
    inspections = [inspection({ effective_authentication: { method: "api_key", source: "environment:NOVA_KEY" } })];
    const view = render(<Harness initial={{ account: config }} />);
    const card = await screen.findByRole("region", { name: "Connection account" });
    expect(within(card).getByRole("combobox", { name: "Environment variable" })).toHaveValue("NOVA_KEY");
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/account/validate", expect.objectContaining({
      config: expect.objectContaining({ api_key: "${NOVA_KEY}" }), credential: { source: "environment", variable: "NOVA_KEY" },
    })));
    view.unmount();
    inspections = [];
    catalog.providers[0].auth_methods = [{ ...brand.auth_methods[0], id: "ambient", credential_method: "aws", interaction: "ambient_credentials",
      fields: [{ id: "region", label: "Region", target: { kind: "configuration", path: "/aws_region" }, schema: { type: "string", enum: ["fixture-region", "other_region"] }, required: true, sensitive: false, suggested_env: [] }] }];
    render(<Harness />); const ambient = await add();
    await chooseOption(ambient, "Region *", "Fixture region");
    expect(within(ambient).getByRole("combobox", { name: "Region *" })).toHaveValue("Fixture region");
    await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/nova/validate", expect.objectContaining({
      config: expect.objectContaining({ aws_region: "fixture-region" }), credential: { source: "ambient", method: "aws" },
    })));
  });
});
