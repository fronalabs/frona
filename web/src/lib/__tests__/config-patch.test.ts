import { describe, expect, it, vi } from "vitest";
import { api } from "../api-client";
import { updateConfig, type Config } from "../config-types";

vi.mock("../api-client", () => ({ api: { put: vi.fn().mockResolvedValue({}) } }));

describe("configuration patch preparation", () => {
  it("keeps arbitrary raw JSON while removing known secret placeholders", async () => {
    const raw = { nested: { is_set: true }, array: [null, { is_set: false }],
      "reasoning.effort": "literal", auth: { encryption_secret: { is_set: true } } };
    await updateConfig({ auth: { encryption_secret: { is_set: true } }, providers: { account: { api_key: { is_set: true } } },
      models: { primary: { extra_params: raw, fallbacks: [{ provider: "account", model: "b", extra_params: { is_set: true } }] } },
    }, { expectedPersistedRevision: "revision" });
    expect(api.put).toHaveBeenLastCalledWith("/api/config", {
      patch: { auth: {}, providers: { account: {} }, models: { primary: { extra_params: raw,
        fallbacks: [{ provider: "account", model: "b", extra_params: { is_set: true } }] } } },
      expected_persisted_revision: "revision",
    });
  });

  it("does not turn dotted literal keys into sensitive field paths", async () => {
    await updateConfig({ "auth.encryption_secret": { is_set: true }, models: { primary: { extra_params: {} } } });
    expect(api.put).toHaveBeenLastCalledWith("/api/config", {
      "auth.encryption_secret": { is_set: true }, models: { primary: { extra_params: {} } },
    });
  });

  it("omits unchanged values and edits reverted to the loaded value", async () => {
    const baseline = { server: { port: 3001, backend_url: "http://backend:3001", timezone: "UTC" },
      auth: { encryption_secret: { is_set: true }, allow_registration: true } } as Config;
    await updateConfig({ server: { ...baseline.server }, auth: { ...baseline.auth } }, {
      expectedPersistedRevision: "revision", baseline,
    });
    expect(api.put).toHaveBeenLastCalledWith("/api/config", {
      patch: {}, expected_persisted_revision: "revision",
    });
  });

  it("keeps explicit resets and sends arrays and raw parameters as complete replacements", async () => {
    const baseline = { server: { port: 4321, external_url: "http://old" }, models: { primary: {
      provider: "account", model: "primary", extra_params: { kept: 1, removed: 2 },
      fallbacks: [{ provider: "account", model: "a", temperature: 0.5 }, { provider: "account", model: "b" }],
    } } } as unknown as Config;
    const fallbacks = [{ provider: "account", model: "b" }, { provider: "account", model: "a", temperature: 0.5 }];
    const patch = { server: { port: 3001, external_url: null }, models: { primary: {
      extra_params: { kept: 1 }, fallbacks,
    } } };
    await updateConfig(patch, { expectedPersistedRevision: "revision", baseline });
    expect(api.put).toHaveBeenLastCalledWith("/api/config", {
      patch, expected_persisted_revision: "revision",
    });
    await updateConfig({ models: { primary: { extra_params: {} } } }, { expectedPersistedRevision: "revision", baseline });
    expect(api.put).toHaveBeenLastCalledWith("/api/config", {
      patch: { models: { primary: { extra_params: {} } } }, expected_persisted_revision: "revision",
    });
  });

  it("keeps a new optional section even when enabled with an empty object", async () => {
    await updateConfig({ browser: {} }, { expectedPersistedRevision: "revision", baseline: { browser: null } as Config });
    expect(api.put).toHaveBeenLastCalledWith("/api/config", {
      patch: { browser: {} }, expected_persisted_revision: "revision",
    });
  });
});
