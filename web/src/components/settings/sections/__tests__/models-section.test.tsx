import React, { useState } from "react";
import { beforeEach, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { ModelsSection } from "../models-section";
import { api } from "@/lib/api-client";
import { modelGroupsPatch } from "@/lib/model-authoring";
import type { ModelGroupConfig, ModelProviderConfig } from "@/lib/config-types";
import type { ModelDirectory, ModelSettingInfo, ProviderProtocol } from "@/lib/provider-admin";
import { providerFingerprint, type ProviderDrafts } from "@/lib/provider-drafts";

vi.mock("@/lib/api-client", async original => ({ ...await original<object>(), api: { get: vi.fn(), post: vi.fn(), put: vi.fn() } }));
const configs = { account: { provider: "unseen-brand", enabled: true, api_key: null, base_url: "http://fixture.invalid" } };
const typed = (key: string, schema: Record<string, unknown>, request = key): ModelSettingInfo => ({
  id: key, label: key, storage: { kind: "typed", config_path: `/${key}` }, request_path: `/${request}`,
  catalog_path: request, description: null, group: "sampling", scope: "provider_request", schema,
  applicability: null, support: "typed", sources: ["fixture exact catalog"],
});
function directory(ids = ["exact"]): ModelDirectory {
  return { connection: "account", credential_method: "api_key", access_mode: "api", source: "account",
    directory_status: "live", source_status: {}, manual_entry: true,
    models: ids.map(id => ({ id, name: id, description: null, context_window: 500, max_tokens: 50,
      availability: id === "exact" ? "account" : "unverified", configured_in: [], sources: ["fixture"], warnings: [],
      suggested_protocol: "responses", capabilities: { reasoning: null, tool_call: null, structured_output: null, input: [], output: [] },
      protocols: (["completions", "responses"] as ProviderProtocol[]).map(api => ({ api, available: true, warnings: [], settings: [
        typed("temperature", { type: "number", minimum: 0, maximum: 2, ...(id === "exact" ? { default: 0.75 } : {}) }),
        ...(api === "completions" ? [typed("top_p", { type: "number", minimum: 0, maximum: 1 })] : []),
        { ...typed("conditional", { type: "boolean" }), storage: { kind: "extra_params" as const, config_path: "/extra_params/conditional" },
          support: "extra_params" as const, applicability: { op: "in" as const, config_path: "/temperature", values: [0] } },
        { ...typed("extra_params", { type: "object", "x-frona-reserved-paths": ["/model", "/messages", "/stream", "/tools"] }),
          request_path: null, catalog_path: null, support: "configured_unverified" as const },
      ] })),
    })),
  };
}
function Harness({ initial, drafts, enabledProviders = ["account"], providerConfigs = configs, savedProviderConfigs = configs }: {
  initial: Record<string, ModelGroupConfig>; drafts?: ProviderDrafts; enabledProviders?: string[];
  providerConfigs?: Record<string, ModelProviderConfig>; savedProviderConfigs?: Record<string, ModelProviderConfig>;
}) {
  const [models, setModels] = useState(initial);
  const [block, setBlock] = useState<string | null>(null);
  return <><ModelsSection models={models} providerConfigs={providerConfigs} savedProviderConfigs={savedProviderConfigs} providerDrafts={drafts}
    enabledProviders={enabledProviders} onChange={setModels} onReadyChange={setBlock} />
    <output data-testid="draft">{JSON.stringify(models)}</output><output data-testid="patch">{JSON.stringify(modelGroupsPatch(initial, models))}</output>
    <button disabled={!!block}>Save fixture</button>{block && <p data-testid="block">{block}</p>}</>;
}
const draft = () => JSON.parse(screen.getByTestId("draft").textContent!);
const primary = () => within(screen.getByRole("group", { name: "Model group primary" }));
async function loaded() {
  fireEvent.click(primary().getByRole("button", { name: "Primary parameters" }));
  await waitFor(() => expect(primary().getByLabelText("temperature")).toBeInTheDocument());
}
async function choose(scope: ReturnType<typeof within>, label: string, option: string) {
  fireEvent.keyDown(scope.getByRole("combobox", { name: label }), { key: "ArrowDown" });
  fireEvent.click(await scope.findByRole("option", { name: option }));
}
beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config/schema"
    ? { $defs: { RetryConfig: { properties: { max_retries: { type: "integer", minimum: 0, default: 10 } } } } }
    : directory(["exact", "unknown"]));
  vi.mocked(api.post).mockResolvedValue(directory());
});

it("loads models as soon as a new connection receives a saved credential", async () => {
  const initial = { primary: { provider: "account", model: "" } };
  const view = render(<Harness initial={initial} savedProviderConfigs={{}} />);
  expect(screen.queryByText(/Validate the provider draft before model discovery/)).not.toBeInTheDocument();
  expect(screen.queryByText(/Saved models remain editable/)).not.toBeInTheDocument();
  expect(api.post).not.toHaveBeenCalled();
  const authenticated = { account: { ...configs.account, credential_id: "00000000-0000-0000-0000-000000000001" } };
  view.rerender(<Harness initial={initial} providerConfigs={authenticated} savedProviderConfigs={{}} />);
  await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/account/models", {
    config: authenticated.account, manual_models: [],
  }));
  await choose(primary(), "Model", "exact");
  expect(draft().primary).toEqual({ provider: "account", model: "exact", api: "responses" });
  expect(api.get).not.toHaveBeenCalledWith(expect.stringContaining("/providers/account/models"));
  expect(api.post).not.toHaveBeenCalledWith(expect.stringContaining("/validate"), expect.anything());
});

it.each([false, true])("loads models from saved settings with an API-key masking marker: %s", async isSet => {
  const saved = { account: { ...configs.account,
    credential_id: "00000000-0000-0000-0000-000000000001", api_key: { is_set: isSet },
  } };
  vi.mocked(api.post).mockImplementation(async (_path, body) => {
    const request = body as { config: { api_key?: unknown } };
    if (request.config.api_key !== null && typeof request.config.api_key === "object") {
      throw new Error("invalid provider request; check the request fields and types");
    }
    return directory();
  });
  render(<Harness initial={{ primary: { provider: "account", model: "" } }}
    providerConfigs={saved} savedProviderConfigs={saved} />);
  await waitFor(() => expect(api.post).toHaveBeenCalledWith("/api/config/providers/account/models", {
    config: { ...saved.account, api_key: null }, manual_models: [],
  }));
  await choose(primary(), "Model", "exact");
  expect(draft().primary).toEqual({ provider: "account", model: "exact", api: "responses" });
  expect(screen.queryByText("invalid provider request; check the request fields and types")).not.toBeInTheDocument();
  expect(saved.account.api_key).toEqual({ is_set: isSet });
});

it.each([[], ["account"], ["account", "second"]])("defaults empty group and fallback providers only with one available connection: %j", async (...enabledProviders: string[]) => {
  render(<Harness enabledProviders={enabledProviders} initial={{
    primary: { provider: "", model: "manual", fallbacks: [{ provider: "", model: "" }] },
    coding: { provider: "saved-provider", model: "saved-model" },
  }} />);
  await waitFor(() => expect(draft().primary.provider).toBe(enabledProviders.length === 1 ? "account" : ""));
  expect(draft().primary.fallbacks[0].provider).toBe(enabledProviders.length === 1 ? "account" : "");
  expect(draft().primary.model).toBe("manual");
  expect(draft().coding).toEqual({ provider: "saved-provider", model: "saved-model" });
});

it("restores collapsible cards, optional group toggles, custom names and gear dialogs", async () => {
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "responses", temperature: 0 } }} />);
  expect(screen.getByText("Required")).toBeInTheDocument();
  expect(screen.getAllByText("Uses Primary")).toHaveLength(3);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(screen.queryByText("Main Model")).not.toBeInTheDocument();
  expect(primary().queryByLabelText("temperature")).not.toBeInTheDocument();
  fireEvent.click(primary().getByRole("button", { name: /Primary\s*Required/ }));
  expect(primary().queryByRole("combobox", { name: "Model" })).not.toBeInTheDocument();
  fireEvent.click(primary().getByRole("button", { name: /Primary\s*Required/ }));
  await loaded();
  expect(screen.getByRole("dialog", { name: "Primary parameters" })).toBeInTheDocument();
  expect(primary().getByLabelText("temperature")).toHaveValue(0);
  fireEvent.keyDown(document, { key: "Escape" });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Enable Coding model group" }));
  expect(screen.getByRole("group", { name: "Model group coding" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("switch", { name: "Disable Coding model group" }));
  expect(screen.queryByRole("group", { name: "Model group coding" })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "+ Add Model Group" }));
  const name = screen.getByRole("combobox", { name: "Group ID" });
  fireEvent.change(name, { target: { value: "My Group" } });
  fireEvent.blur(name);
  expect(screen.getByRole("group", { name: "Model group my_group" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("switch", { name: "Disable My Group model group" }));
  fireEvent.click(screen.getByRole("button", { name: "Delete" }));
  expect(screen.queryByRole("group", { name: "Model group my_group" })).not.toBeInTheDocument();
  expect(draft().primary).toEqual({ provider: "account", model: "exact", api: "responses", temperature: 0 });
});

it("keeps two groups independent, preserves unsupported values, and switches protocols without a new live list", async () => {
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "completions", temperature: 0, top_p: 0.8 },
    coding: { provider: "account", model: "exact", api: "responses", temperature: 1.5 } }} />);
  await loaded();
  const calls = vi.mocked(api.get).mock.calls.length;
  await choose(primary(), "Protocol", "Responses");
  expect(draft().primary).toMatchObject({ api: "responses", temperature: 0, top_p: 0.8 });
  expect(draft().coding).toMatchObject({ api: "responses", temperature: 1.5 });
  expect(api.get).toHaveBeenCalledTimes(calls);
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeDisabled());
  fireEvent.click(primary().getByRole("button", { name: "Clear /top_p" }));
  expect(JSON.parse(screen.getByTestId("patch").textContent!).primary).toEqual({ api: "responses", top_p: null });
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
});

it.each([false, true])("selects DeepSeek's protocol after changing a provider (fallback: %s)", async fallback => {
  const deepseek = { ...configs.account, provider: "deepseek" };
  const providers = { ...configs, deepseek };
  const target = directory(["deepseek-chat"]);
  target.connection = "deepseek";
  target.models[0].suggested_protocol = "completions";
  target.models[0].protocols = target.models[0].protocols.filter(protocol => protocol.api === "completions");
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config/schema" ? {}
    : path.includes("/deepseek/models") ? target : directory());
  const original: ModelGroupConfig = { provider: "account", model: "exact", api: "responses", temperature: 0 };
  render(<Harness initial={{ primary: fallback ? { ...original, fallbacks: [{ ...original }] } : original }}
    enabledProviders={["account", "deepseek"]} providerConfigs={providers} savedProviderConfigs={providers} />);
  const scope = fallback ? within(screen.getByRole("group", { name: "Fallback 1" })) : primary();
  await choose(scope, "Provider", "DeepSeek");
  await choose(scope, "Model", "deepseek-chat");
  await waitFor(() => expect(fallback ? draft().primary.fallbacks[0] : draft().primary).toMatchObject({
    provider: "deepseek", model: "deepseek-chat", api: "completions", temperature: 0,
  }));
  expect(scope.queryByText("Reconcile unsupported settings: /api")).not.toBeInTheDocument();
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
  if (fallback) expect(draft().primary.api).toBe("responses");
});

it.each([false, true])("resets incompatible settings only when selecting another model (fallback: %s)", async fallback => {
  const data = directory(["exact", "other"]);
  for (const row of data.models) for (const protocol of row.protocols) {
    protocol.settings.push(typed("reasoning_effort", { type: "string", allOf: [{ enum: ["low", "high"] }] }));
  }
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config/schema" ? {} : data);
  const original: ModelGroupConfig = { provider: "account", model: "exact", api: "responses", temperature: 0,
    reasoning_effort: "xhigh", top_p: 0.8, retry: { max_retries: 2, initial_backoff_ms: 100, backoff_multiplier: 2, max_backoff_ms: 1000 }, extra_params: { custom: { nested: [null] } } };
  render(<Harness initial={{ primary: fallback ? { ...original, reasoning_effort: "high", top_p: null, fallbacks: [{ ...original }] } : original }} />);
  const scope = fallback ? within(screen.getByRole("group", { name: "Fallback 1" })) : primary();
  const selected = () => fallback ? draft().primary.fallbacks[0] : draft().primary;
  await waitFor(() => expect(scope.getByRole("combobox", { name: "Model" })).toHaveAttribute("placeholder", "Select or enter model"));
  expect(selected().reasoning_effort).toBe("xhigh");
  await choose(scope, "Model", "other");
  await waitFor(() => expect(selected().reasoning_effort).toBeUndefined());
  expect(selected().top_p).toBeUndefined();
  expect(selected()).toMatchObject({ model: "other", temperature: 0, retry: { max_retries: 2 }, extra_params: original.extra_params });
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
  if (fallback) expect(draft().primary.reasoning_effort).toBe("high");
});

it("waits for a manually selected model's settings before resetting incompatible values", async () => {
  let finishDiscovery!: (value: ModelDirectory) => void;
  const pending = new Promise<ModelDirectory>(resolve => { finishDiscovery = resolve; });
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config/schema" ? {}
    : path.includes("manual-new") ? pending : directory());
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "responses", reasoning_effort: "xhigh" } }} />);
  await waitFor(() => expect(primary().getByRole("combobox", { name: "Model" })).toHaveAttribute("placeholder", "Select or enter model"));
  fireEvent.change(primary().getByRole("combobox", { name: "Model" }), { target: { value: "manual-new" } });
  fireEvent.blur(primary().getByRole("combobox", { name: "Model" }));
  await waitFor(() => expect(api.get).toHaveBeenCalledWith(expect.stringContaining("manual-new")));
  expect(draft().primary.reasoning_effort).toBe("xhigh");
  const data = directory(["manual-new"]);
  for (const protocol of data.models[0].protocols) protocol.settings.push(typed("reasoning_effort", { type: "string", enum: ["low", "high"] }));
  await act(async () => { finishDiscovery(data); });
  await waitFor(() => expect(draft().primary.reasoning_effort).toBeUndefined());
  expect(draft().primary.model).toBe("manual-new");
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
});

it("does not copy limits or defaults until accepted and never overwrites saved values on refresh", async () => {
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "responses" } }} />);
  await loaded();
  expect(draft().primary).toEqual({ provider: "account", model: "exact", api: "responses" });
  fireEvent.click(primary().getByRole("button", { name: "Use suggested 0.75" }));
  expect(draft().primary.temperature).toBe(0.75);
  const changed = directory();
  changed.models[0].protocols[1].settings[0].schema.default = 1.2;
  vi.mocked(api.get).mockResolvedValue(changed);
  await act(async () => fireEvent.click(primary().getByRole("button", { name: "Refresh descriptions" })));
  expect(draft().primary.temperature).toBe(0.75);
  expect(draft().primary.max_tokens).toBeUndefined();
  expect(draft().primary.context_window).toBeUndefined();
  fireEvent.click(primary().getByRole("button", { name: "Reset temperature to default" }));
  expect(draft().primary.temperature).toBeUndefined();
  expect(primary().queryByRole("button", { name: "Reset temperature to default" })).not.toBeInTheDocument();
});

it("adds primitive values, rejects objects, preserves complex YAML, and sends full replacement objects on deletion and clear", async () => {
  const complex = { nested: [1, null, { enabled: true }] };
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "responses", extra_params: { complex, obsolete: false } } }} />);
  await loaded();
  expect(primary().getByRole("button", { name: "Custom request parameters" })).toHaveAttribute("aria-expanded", "false");
  expect(primary().queryByLabelText("Custom key")).not.toBeInTheDocument();
  fireEvent.click(primary().getByRole("button", { name: "Custom request parameters" }));
  fireEvent.change(primary().getByLabelText("Custom key"), { target: { value: "a.b/c" } });
  fireEvent.change(primary().getByLabelText("Custom value"), { target: { value: "{}" } });
  fireEvent.click(primary().getByRole("button", { name: "Add custom parameter" }));
  expect(primary().getByText("Objects and arrays cannot be entered here. Use YAML for complex values.")).toBeInTheDocument();
  expect(draft().primary.extra_params).toEqual({ complex, obsolete: false });
  fireEvent.change(primary().getByLabelText("Custom value"), { target: { value: "null" } });
  fireEvent.click(primary().getByRole("button", { name: "Add custom parameter" }));
  fireEvent.click(primary().getByRole("button", { name: "Delete obsolete" }));
  expect(JSON.parse(screen.getByTestId("patch").textContent!).primary.extra_params).toEqual({ complex, "a.b/c": null });
  expect(primary().getByText("Complex value preserved. Edit its internals in YAML.")).toBeInTheDocument();
  fireEvent.click(primary().getByRole("button", { name: "Clear custom parameters" }));
  expect(JSON.parse(screen.getByTestId("patch").textContent!).primary.extra_params).toEqual({});
});

it("enforces catalog ranges, applicability and reserved paths, and warns about typed overrides", async () => {
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "responses", temperature: 0 } }} />);
  await loaded();
  expect(primary().getByLabelText("conditional")).toBeEnabled();
  fireEvent.change(primary().getByLabelText("temperature"), { target: { value: "3" } });
  expect(primary().getByText("/temperature: must be at most 2")).toBeInTheDocument();
  expect(primary().getByLabelText("conditional")).toBeDisabled();
  fireEvent.change(primary().getByLabelText("temperature"), { target: { value: "0" } });
  fireEvent.click(primary().getByRole("button", { name: "Custom request parameters" }));
  const add = (key: string, value: string) => {
    fireEvent.change(primary().getByLabelText("Custom key"), { target: { value: key } });
    fireEvent.change(primary().getByLabelText("Custom value"), { target: { value } });
    fireEvent.click(primary().getByRole("button", { name: "Add custom parameter" }));
  };
  add("model", '"other"');
  expect(primary().getByText("/extra_params/model: reserved request field")).toBeInTheDocument();
  add("temperature", "4");
  expect(primary().getByText("/extra_params/temperature: must be at most 2")).toBeInTheDocument();
  add("temperature", "1");
  expect(primary().getByText("/extra_params/temperature overrides /temperature")).toBeInTheDocument();
  expect(primary().getByLabelText("conditional")).toBeDisabled();
  add("unknown-option", "false");
  expect(draft().primary.extra_params["unknown-option"]).toBe(false);
});

it("supports manual IDs, draft-proof listing, explicit protocols for new fallbacks, and independent fallback edits", async () => {
  const proof = { config: configs.account, source: "database", method: "api_key" as const, validation_id: "exact-draft-proof",
    fingerprint: providerFingerprint(configs.account), expiresAt: Date.now() + 60_000 };
  vi.mocked(api.post).mockResolvedValue(directory(["exact", "unknown"]));
  render(<Harness drafts={{ account: proof }} initial={{ primary: { provider: "account", model: "exact", api: "completions", temperature: 0.5 } }} />);
  await loaded();
  expect(api.post).toHaveBeenCalledWith("/api/config/providers/account/models", expect.objectContaining({ validation_id: "exact-draft-proof" }));
  fireEvent.click(primary().getByRole("button", { name: "Close" }));
  fireEvent.click(primary().getByRole("button", { name: "Add fallback" }));
  const fallback = within(screen.getByRole("group", { name: "Fallback 1" }));
  await choose(fallback, "Provider", "Unseen Brand (account)");
  fireEvent.change(fallback.getByRole("combobox", { name: "Model" }), { target: { value: "unknown" } });
  fireEvent.blur(fallback.getByRole("combobox", { name: "Model" }));
  expect(draft().primary.fallbacks[0]).toEqual({ provider: "account", model: "unknown", api: "responses" });
  fireEvent.click(fallback.getByRole("button", { name: "Primary fallback 1 parameters" }));
  fireEvent.change(fallback.getByLabelText("temperature"), { target: { value: "0" } });
  expect(draft().primary.temperature).toBe(0.5);
  expect(draft().primary.fallbacks[0].temperature).toBe(0);
  expect(fallback.queryByText("Use suggested 0.75")).not.toBeInTheDocument();
});

it("retains saved IDs and settings when discovery fails and when catalogs have no exact match", async () => {
  vi.mocked(api.get).mockImplementation(async path => {
    if (path === "/api/config/schema") return {};
    throw new Error("inventory unreachable");
  });
  render(<Harness initial={{ primary: { provider: "account", model: "saved-unknown", api: "responses", temperature: 0.3, extra_params: { x: [null] } } }} />);
  await screen.findByText(/inventory unreachable/);
  expect(primary().getByRole("combobox", { name: "Model" })).toHaveValue("saved-unknown");
  expect(draft().primary.extra_params).toEqual({ x: [null] });
  vi.mocked(api.get).mockResolvedValue({ ...directory(["saved-unknown"]), directory_status: "live_error" });
  fireEvent.click(primary().getByRole("button", { name: "Primary parameters" }));
  await act(async () => fireEvent.click(primary().getByRole("button", { name: "Refresh descriptions" })));
  expect(primary().getByLabelText("temperature")).toHaveValue(0.3);
});

it("uses retry schema constraints and requires explicit reconciliation of an unsupported saved protocol", async () => {
  const data = directory();
  data.models[0].protocols.push({ api: "anthropic-messages", available: false, settings: [], warnings: ["unsupported_protocol_for_adapter"] });
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config/schema"
    ? { $defs: { RetryConfig: { properties: { max_retries: { type: "integer", minimum: 0, default: 10 } } } } }
    : data);
  render(<Harness initial={{ primary: { provider: "account", model: "exact", api: "anthropic-messages" } }} />);
  fireEvent.click(primary().getByRole("button", { name: "Primary parameters" }));
  await screen.findByText("/api is unsupported by this protocol.");
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeDisabled());
  await choose(primary(), "Protocol", "Responses");
  fireEvent.click(primary().getByRole("button", { name: "Retry" }));
  fireEvent.change(primary().getByLabelText("max retries"), { target: { value: "-1" } });
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeDisabled());
  fireEvent.change(primary().getByLabelText("max retries"), { target: { value: "0" } });
  await waitFor(() => expect(screen.getByRole("button", { name: "Save fixture" })).toBeEnabled());
  expect(draft().primary.retry).toEqual({ max_retries: 0 });
});
