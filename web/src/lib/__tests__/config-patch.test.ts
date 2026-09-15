import { describe, expect, it, vi } from "vitest";
import { api } from "../api-client";
import { updateConfig } from "../config-types";

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
});
