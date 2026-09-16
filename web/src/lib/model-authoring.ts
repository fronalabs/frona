import type { ModelGroupConfig } from "./config-types";
import type { ModelSettingInfo, ProviderModelRow, SettingApplicability } from "./provider-admin";
import { readPointer } from "./provider-drafts";

export type ProtocolInfo = ProviderModelRow["protocols"][number];
export type Schema = Record<string, unknown>;
export const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value);
const equal = (left: unknown, right: unknown) => JSON.stringify(left) === JSON.stringify(right);
export const pointerKey = (key: string) => key.replace(/~/g, "~0").replace(/\//g, "~1");

export function parseCustomPrimitive(text: string): string | number | boolean | null {
  const trimmed = text.trim();
  let value: unknown;
  try { value = JSON.parse(trimmed); } catch { return trimmed; }
  if (value !== null && typeof value === "object") throw new Error("Objects and arrays cannot be entered here. Use YAML for complex values.");
  if (typeof value === "number" && (!Number.isFinite(value) || Number.isInteger(value) && !Number.isSafeInteger(value))) throw new Error("This number is outside the browser's safe range. Use YAML.");
  return value as string | number | boolean | null;
}

export function removePointer<T extends object>(value: T, path: string): T {
  const parts = path.slice(1).split("/").map(part => part.replace(/~1/g, "/").replace(/~0/g, "~"));
  function remove(current: Record<string, unknown>, index: number): Record<string, unknown> {
    const next = { ...current }, key = parts[index];
    if (index === parts.length - 1) delete next[key];
    else if (Object.hasOwn(next, key) && object(next[key])) next[key] = remove(next[key], index + 1);
    return next;
  }
  return remove(value as Record<string, unknown>, 0) as T;
}

/** Null deletion markers apply to typed settings; raw objects replace whole. */
export function modelGroupsPatch(before: Record<string, ModelGroupConfig>, after: Record<string, ModelGroupConfig>): Record<string, unknown> {
  function diff(before: Record<string, unknown>, after: Record<string, unknown>): Record<string, unknown> {
    const result: Record<string, unknown> = {};
    for (const key of new Set([...Object.keys(before), ...Object.keys(after)])) {
      if (!Object.hasOwn(after, key)) result[key] = key === "extra_params" ? {} : null;
      else if (!equal(before[key], after[key])) {
        result[key] = key !== "extra_params" && object(before[key]) && object(after[key]) ? diff(before[key], after[key]) : after[key];
      }
    }
    return result;
  }
  return diff(before, after);
}

export function combineModelPatches(previous: Record<string, unknown>, next: Record<string, unknown>): Record<string, unknown> {
  const result = { ...previous };
  for (const [key, value] of Object.entries(next)) result[key] = key !== "extra_params" && object(result[key]) && object(value) ? combineModelPatches(result[key], value) : value;
  return result;
}

export function resolveSchema(root: unknown, value: unknown): Schema {
  if (!object(value)) return {};
  if (typeof value.$ref === "string" && value.$ref.startsWith("#/")) return resolveSchema(root, readPointer(root, value.$ref.slice(1)));
  if (Array.isArray(value.anyOf)) {
    const nonNull = value.anyOf.find(option => object(option) && option.type !== "null");
    if (nonNull) return resolveSchema(root, nonNull);
  }
  return value;
}

export function schemaError(value: unknown, schema: Schema): string | null {
  if (Array.isArray(schema.allOf)) for (const rule of schema.allOf) { const error = schemaError(value, object(rule) ? rule : {}); if (error) return error; }
  if (Array.isArray(schema.anyOf) && !schema.anyOf.some(rule => !schemaError(value, object(rule) ? rule : {}))) return "does not match an allowed type";
  if (Array.isArray(schema.enum) && !schema.enum.some(option => equal(option, value))) return "is not an allowed value";
  if (schema.const !== undefined && !equal(value, schema.const)) return "does not match the required value";
  const types = typeof schema.type === "string" ? [schema.type] : Array.isArray(schema.type) ? schema.type : [];
  const matches = (type: unknown) => type === "null" ? value === null : type === "object" ? object(value) : type === "array" ? Array.isArray(value)
    : type === "integer" ? typeof value === "number" && Number.isSafeInteger(value) : typeof value === type;
  if (types.length && !types.some(matches)) return `must be ${types.join(" or ")}`;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return "must be a finite number";
    if (typeof schema.minimum === "number" && value < schema.minimum) return `must be at least ${schema.minimum}`;
    if (typeof schema.maximum === "number" && value > schema.maximum) return `must be at most ${schema.maximum}`;
    if (typeof schema.exclusiveMinimum === "number" && value <= schema.exclusiveMinimum) return `must exceed ${schema.exclusiveMinimum}`;
    if (typeof schema.exclusiveMaximum === "number" && value >= schema.exclusiveMaximum) return `must be less than ${schema.exclusiveMaximum}`;
    if (typeof schema.multipleOf === "number" && Math.abs(value / schema.multipleOf - Math.round(value / schema.multipleOf)) > 1e-8) return `must use increments of ${schema.multipleOf}`;
  }
  if (typeof value === "string") {
    if (typeof schema.minLength === "number" && [...value].length < schema.minLength) return `must contain at least ${schema.minLength} characters`;
    if (typeof schema.maxLength === "number" && [...value].length > schema.maxLength) return `must contain at most ${schema.maxLength} characters`;
    if (typeof schema.pattern === "string") { try { if (!new RegExp(schema.pattern).test(value)) return "does not match the required pattern"; } catch { return "has an unsupported schema pattern"; } }
  }
  if (Array.isArray(value)) {
    if (typeof schema.minItems === "number" && value.length < schema.minItems) return `requires at least ${schema.minItems} entries`;
    if (typeof schema.maxItems === "number" && value.length > schema.maxItems) return `allows at most ${schema.maxItems} entries`;
    if (object(schema.items)) for (const item of value) { const error = schemaError(item, schema.items); if (error) return error; }
  }
  if (object(value)) {
    if (Array.isArray(schema.required)) for (const key of schema.required) if (typeof key === "string" && !Object.hasOwn(value, key)) return `requires ${key}`;
    if (object(schema.properties)) for (const [key, property] of Object.entries(schema.properties)) {
      if (Object.hasOwn(value, key) && object(property)) { const error = schemaError(value[key], property); if (error) return `${key} ${error}`; }
    }
  }
  return null;
}

function effectiveValue(group: ModelGroupConfig, path: string, settings: ModelSettingInfo[]): unknown {
  const setting = settings.find(setting => setting.storage?.config_path === path);
  if (setting?.request_path) {
    const raw = readPointer(group, `/extra_params${setting.request_path}`);
    if (raw !== undefined) return raw;
  }
  return readPointer(group, path) ?? setting?.schema.default;
}

export function applicable(expression: SettingApplicability | null, group: ModelGroupConfig, settings: ModelSettingInfo[]): boolean {
  if (!expression) return true;
  if (expression.op === "all") return expression.expressions.every(rule => applicable(rule, group, settings));
  if (expression.op === "any") return expression.expressions.some(rule => applicable(rule, group, settings));
  if (expression.op === "not") return !applicable(expression.expression, group, settings);
  return expression.values.some(value => equal(effectiveValue(group, expression.config_path, settings), value));
}

export function unsupportedPaths(group: ModelGroupConfig, protocol?: ProtocolInfo): string[] {
  if (!protocol) return group.api ? ["/api"] : [];
  const paths = protocol.settings.flatMap(setting => setting.storage?.kind === "typed" ? [setting.storage.config_path] : []);
  const errors: string[] = protocol.settings.length === 0 && group.api ? ["/api"] : [];
  function visit(value: unknown, path: string) {
    if (value === null || value === undefined || paths.includes(path)) return;
    if (object(value) && Object.keys(value).length) for (const [key, child] of Object.entries(value)) visit(child, `${path}/${pointerKey(key)}`);
    else if (!paths.includes(path)) errors.push(path);
  }
  for (const [key, value] of Object.entries(group)) if (!["provider", "model", "api", "fallbacks", "retry", "extra_params"].includes(key)) visit(value, `/${pointerKey(key)}`);
  return errors;
}

export function modelSettingErrors(group: ModelGroupConfig, protocol?: ProtocolInfo): string[] {
  if (!protocol) return [];
  const errors = unsupportedPaths(group, protocol).map(path => `${path}: unsupported by this protocol`);
  for (const setting of protocol.settings) {
    if (!setting.storage || setting.storage.config_path === "/extra_params") continue;
    const value = readPointer(group, setting.storage.config_path);
    if (value === undefined || value === null && setting.storage.kind === "typed") continue;
    const error = schemaError(value, setting.schema);
    if (error) errors.push(`${setting.storage.config_path}: ${error}`);
    if (!applicable(setting.applicability, group, protocol.settings)) errors.push(`${setting.storage.config_path}: not applicable with the selected settings`);
  }
  const rawEditor = protocol.settings.find(setting => setting.storage?.config_path === "/extra_params");
  const reserved = rawEditor?.schema["x-frona-reserved-paths"];
  if (Array.isArray(reserved)) for (const path of reserved) {
    if (typeof path !== "string") continue;
    let value: unknown = group.extra_params ?? {};
    let touched = true;
    for (const token of path.slice(1).split("/")) {
      if (!object(value)) break;
      const key = token.replace(/~1/g, "/").replace(/~0/g, "~");
      if (!Object.hasOwn(value, key)) { touched = false; break; }
      value = value[key];
    }
    if (touched) errors.push(`/extra_params${path}: reserved request field`);
  }
  // Raw overrides follow the same exact catalog rule as their typed setting.
  for (const setting of protocol.settings) if (setting.catalog_path && setting.request_path) {
    const value = readPointer(group, `/extra_params${setting.request_path}`);
    if (value === undefined) continue;
    const error = schemaError(value, setting.schema);
    if (error) errors.push(`/extra_params${setting.request_path}: ${error}`);
    if (!applicable(setting.applicability, group, protocol.settings)) errors.push(`/extra_params${setting.request_path}: not applicable with the selected settings`);
  }
  return [...new Set(errors)];
}

/** Reconcile an explicit model selection, never a background metadata refresh. */
export function reconcileModelSettings(group: ModelGroupConfig, protocol: ProtocolInfo): ModelGroupConfig {
  if (!protocol.available || !protocol.settings.length) return group;
  let next = group;
  while (true) {
    const invalid = new Set(unsupportedPaths(next, protocol).filter(path => path !== "/api"));
    for (const setting of protocol.settings) {
      const paths = new Set<string>();
      if (setting.storage && setting.storage.config_path !== "/extra_params") paths.add(setting.storage.config_path);
      if (setting.catalog_path && setting.request_path) paths.add(`/extra_params${setting.request_path}`);
      for (const path of paths) {
        const value = readPointer(next, path);
        if (value === undefined || value === null && !path.startsWith("/extra_params/")) continue;
        if (schemaError(value, setting.schema)) invalid.add(path);
      }
    }
    // Resolve types and bounds first; their defaults may restore applicability.
    if (!invalid.size) for (const setting of protocol.settings) {
      if (applicable(setting.applicability, next, protocol.settings)) continue;
      const paths = [setting.storage?.config_path,
        setting.catalog_path && setting.request_path ? `/extra_params${setting.request_path}` : undefined];
      for (const path of paths) if (path && path !== "/extra_params" && readPointer(next, path) !== undefined) invalid.add(path);
    }
    if (!invalid.size) return next;
    for (const path of invalid) next = removePointer(next, path);
  }
}

export function overrideWarnings(group: ModelGroupConfig, protocol?: ProtocolInfo): string[] {
  return protocol?.settings.flatMap(setting => setting.storage?.kind === "typed" && setting.request_path
    && readPointer(group, setting.storage.config_path) != null && readPointer(group, `/extra_params${setting.request_path}`) !== undefined
    ? [`/extra_params${setting.request_path} overrides ${setting.storage.config_path}`] : []) ?? [];
}
