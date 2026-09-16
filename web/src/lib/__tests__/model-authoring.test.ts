import { expect, it } from "vitest";
import { modelGroupsPatch, parseCustomPrimitive, removePointer, schemaError } from "../model-authoring";

it.each([["true", true], ["false", false], ["null", null], ["0", 0], ["1.25", 1.25], ['"true"', "true"], ['"null"', "null"], [" ordinary text ", "ordinary text"]])("parses %s without changing its type", (text, expected) => {
  expect(parseCustomPrimitive(String(text))).toEqual(expected);
});
it.each(["{}", "[]", '{"a":1}', "[true]", "9007199254740993", "1e999"])("rejects unsupported browser input %s", text => {
  expect(() => parseCustomPrimitive(text)).toThrow();
});
it("diffs typed deletion, literal raw null, whole-object replacement and fallback arrays against the persisted baseline", () => {
  const before = { primary: { provider: "account", model: "exact", temperature: 0.5, extra_params: { complex: { nested: [1, null] }, obsolete: false } } };
  const after = { primary: { provider: "account", model: "exact", extra_params: { complex: { nested: [1, null] }, literal: null } } };
  expect(modelGroupsPatch(before, after)).toEqual({ primary: { temperature: null, extra_params: after.primary.extra_params } });
  expect(modelGroupsPatch(before, { primary: { ...before.primary, extra_params: {} } })).toEqual({ primary: { extra_params: {} } });
  expect(modelGroupsPatch(before, {})).toEqual({ primary: null });
  expect(modelGroupsPatch(before, before)).toEqual({});
  expect(modelGroupsPatch(before, { primary: { ...before.primary, fallbacks: [{ provider: "backup", model: "unknown", api: "responses" }] } })).toEqual({
    primary: { fallbacks: [{ provider: "backup", model: "unknown", api: "responses" }] },
  });
});
it("removes literal pointer keys without mutating siblings or complex values", () => {
  const value = { extra_params: { "a/b": null, "a.b": [true, { x: 1 }] } };
  expect(removePointer(value, "/extra_params/a~1b")).toEqual({ extra_params: { "a.b": [true, { x: 1 }] } });
  expect(value.extra_params["a/b"]).toBeNull();
});
it("checks intersected constraints and raw nullable types", () => {
  expect(schemaError(3, { allOf: [{ type: "number", maximum: 5 }, { maximum: 2 }] })).toContain("at most 2");
  expect(schemaError(null, { type: ["number", "null"] })).toBeNull();
  expect(schemaError("long", { type: "string", maxLength: 2 })).toContain("at most 2");
});
