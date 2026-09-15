import { afterEach, expect, it, vi } from "vitest";
import { matchingDraft, normalizeProviderHandle, prepareProviderDraft, readPointer, writePointer } from "../provider-drafts";

afterEach(() => vi.restoreAllMocks());

it("normalizes handles and rejects invalid or overlong identities", () => {
  expect(normalizeProviderHandle(" Work_2 ")).toBe("work_2");
  for (const value of ["x", "2account", "account.name", "x".repeat(33), "\u00f6penai"]) expect(() => normalizeProviderHandle(value)).toThrow();
});

it("filters expired and mismatched proofs without changing their configurations", () => {
  const config = { provider: "openai", base_url: "https://fixture.invalid", api_key: null, enabled: true };
  vi.spyOn(Date, "now").mockReturnValue(1_000);
  const draft = prepareProviderDraft(config, "database", { validation_id: "proof", credential: { method: "api_key", state: "pending", generation: 0, version: "version" }, models: [] });
  expect(matchingDraft(config, draft)).toBe(draft);
  expect(matchingDraft({ ...config, enabled: false }, draft)).toBe(draft);
  expect(matchingDraft({ ...config, base_url: "https://other.invalid" }, draft)).toBeUndefined();
  vi.spyOn(Date, "now").mockReturnValue(draft.expiresAt);
  expect(matchingDraft(config, draft)).toBeUndefined();
  expect(draft.config.base_url).toBe("https://fixture.invalid");
});

it("keeps literal JSON Pointer keys and never traverses inherited objects", () => {
  const original = { extra_params: { existing: null } };
  const next = writePointer(original, "/extra_params/literal.dot/a~1b/~0", false);
  expect(readPointer(next, "/extra_params/literal.dot/a~1b/~0")).toBe(false);
  expect(readPointer(original, "/extra_params/literal.dot")).toBeUndefined();
  expect(readPointer({}, "/constructor/prototype")).toBeUndefined();
  const own = writePointer({}, "/__proto__/fixture", true);
  expect(Object.hasOwn(own, "__proto__")).toBe(true);
  expect(readPointer({}, "/fixture")).toBeUndefined();
});
