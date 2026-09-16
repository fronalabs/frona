"use client";
import { useId, useState } from "react";
import type { ModelGroupConfig } from "@/lib/config-types";
import type { ModelSettingInfo } from "@/lib/provider-admin";
import { applicable, object, removePointer, schemaError, type ProtocolInfo, type Schema } from "@/lib/model-authoring";
import { readPointer, writePointer } from "@/lib/provider-drafts";
import { ComboboxInput } from "@/components/settings/combobox";
import { Field, InputResetButton, Toggle } from "@/components/settings/field";

const input = "rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none disabled:opacity-50";
const button = "rounded-lg border border-border px-2 py-1 text-xs text-text-secondary hover:bg-surface-tertiary disabled:opacity-50";

function facets(schema: Schema): Schema {
  const rules = Array.isArray(schema.allOf) ? schema.allOf.filter(object) : [];
  const result = Object.assign({}, ...rules, schema);
  for (const [key, combine] of [["minimum", Math.max], ["maximum", Math.min]] as const) {
    const values = [schema, ...rules].map(rule => rule[key]).filter((value): value is number => typeof value === "number");
    if (values.length) result[key] = combine(...values);
  }
  return result;
}

function ParsedInput({ label, value, parse, onCommit, onError, complex = false, onClear }: {
  label: string; value: unknown; parse: (value: string) => unknown; onCommit: (value: unknown) => void;
  onError: (error: string | null) => void; complex?: boolean;
  onClear?: () => void;
}) {
  const inputId = useId();
  const [text, setText] = useState("");
  const [editing, setEditing] = useState(false);
  const display = editing ? text : value === undefined ? "" : JSON.stringify(value);
  function change(text: string) {
    setText(text); setEditing(true);
    try { parse(text); onError(null); } catch (error) { onError(error instanceof Error ? error.message : "Invalid value"); }
  }
  function commit() {
    if (!editing) return;
    try { onCommit(parse(text)); onError(null); setEditing(false); }
    catch (error) { onError(error instanceof Error ? error.message : "Invalid value"); }
  }
  return <Field label={label} htmlFor={inputId}><div className="relative">{complex
    ? <textarea id={inputId} className={`${input} block w-full ${onClear ? "pr-9" : ""}`} value={display} onChange={event => change(event.target.value)} onBlur={commit} />
    : <input id={inputId} className={`${input} block w-full ${onClear ? "pr-9" : ""}`} value={display} onChange={event => change(event.target.value)} onBlur={commit} onKeyDown={event => { if (event.key === "Enter") { event.preventDefault(); commit(); } }} />}
    {onClear && <InputResetButton label={`Reset ${label} to default`} className={`absolute right-2 ${complex ? "top-2" : "top-1/2 -translate-y-1/2"}`}
      onClick={() => { setEditing(false); setText(""); onClear(); }} />}
  </div></Field>;
}

export function SettingControl({ setting, group, settings, onChange, onError }: { setting: ModelSettingInfo; group: ModelGroupConfig;
  settings: ModelSettingInfo[]; onChange: (group: ModelGroupConfig) => void; onError: (path: string, error: string | null) => void }) {
  const inputId = useId();
  const path = setting.storage?.config_path;
  const value = path ? readPointer(group, path) : undefined;
  const schema = facets(setting.schema);
  const writable = !!path && applicable(setting.applicability, group, settings);
  function set(value: unknown) { if (path) onChange(writePointer(group, path, value)); }
  function clear() { if (path) { onChange(removePointer(group, path)); onError(path, null); } }
  const types = typeof schema.type === "string" ? [schema.type] : Array.isArray(schema.type) ? schema.type : [];
  const complexRaw = setting.storage?.kind === "extra_params" && (types.includes("array") || types.includes("object"));
  const options = Array.isArray(schema.enum) ? schema.enum : undefined;
  const reset = path && value !== undefined ? clear : undefined;
  const resetLabel = `Reset ${setting.label} to default`;
  return <div className="space-y-2">
    {options ? <ComboboxInput label={setting.label} description={setting.description ?? undefined} allowFreeText={false}
      value={value === undefined || value === null && setting.storage?.kind === "typed" ? "" : JSON.stringify(value)} disabled={!writable}
      items={[{ value: "", label: "Use default" }, ...options.map(option => ({ value: JSON.stringify(option), label: option === null ? "None (explicit)" : String(option).replaceAll("_", " ").replace(/^./, character => character.toUpperCase()) })),
        ...(value !== undefined && !options.some(option => JSON.stringify(option) === JSON.stringify(value)) ? [{ value: JSON.stringify(value), label: `Saved: ${String(value)}` }] : [])]}
      onChange={value => value === "" ? clear() : set(JSON.parse(value))} onClear={reset} clearLabel={resetLabel} />
      : types.includes("boolean") ? <Toggle label={setting.label} description={setting.description ?? undefined} value={value === true} disabled={!writable} onChange={set}
        action={reset && <InputResetButton label={resetLabel} onClick={reset} />} />
        : types.includes("array") || types.includes("object") ? null
          : <Field label={setting.label} description={setting.description ?? undefined} htmlFor={inputId}>
            <div className="relative"><input id={inputId} className={`${input} block w-full ${reset ? "pr-9" : ""}`} type={types.includes("number") || types.includes("integer") ? "number" : "text"}
              value={typeof value === "string" || typeof value === "number" ? value : ""} disabled={!writable} placeholder="Default"
              min={typeof schema.minimum === "number" ? schema.minimum : undefined} max={typeof schema.maximum === "number" ? schema.maximum : undefined}
              step={typeof schema.multipleOf === "number" ? schema.multipleOf : types.includes("integer") ? 1 : "any"}
              onChange={event => event.target.value === "" ? clear() : set(types.includes("number") || types.includes("integer") ? Number(event.target.value) : event.target.value)} />
              {reset && <InputResetButton label={resetLabel} onClick={reset} className="absolute right-2 top-1/2 -translate-y-1/2" />}
            </div>
          </Field>}
    {(complexRaw || !writable && (types.includes("array") || types.includes("object"))) && <div><div className="relative"><pre className="overflow-auto pr-8 text-xs">{JSON.stringify(value)}</pre>
      {reset && <InputResetButton label={resetLabel} onClick={reset} className="absolute right-0 top-0" />}
    </div><p className="text-xs">Complex value preserved. Edit its internals in YAML.</p></div>}
    {!complexRaw && (types.includes("array") || types.includes("object")) && writable && <ParsedInput label={`${setting.label} JSON`} value={value} complex
      parse={text => { const value: unknown = JSON.parse(text); const error = schemaError(value, setting.schema); if (error) throw new Error(error); return value; }}
      onCommit={set} onError={error => onError(path!, error)} onClear={reset} />}
    {!writable && <p className="text-xs text-warning">{path ? "Not applicable with the selected settings" : "The adapter cannot write this setting"}</p>}
    {writable && !complexRaw && (value === undefined || value === null && setting.storage?.kind === "typed") && Object.hasOwn(schema, "default") && <button className={button} onClick={() => set(schema.default)}>Use suggested {JSON.stringify(schema.default)}</button>}
  </div>;
}

export function ModelSettings({ group, protocol, onChange, onError }: { group: ModelGroupConfig; protocol: ProtocolInfo;
  onChange: (group: ModelGroupConfig) => void; onError: (path: string, error: string | null) => void }) {
  const controls = protocol.settings.filter(setting => setting.storage?.config_path !== "/extra_params" && setting.support !== "configured_unverified");
  return <div className="space-y-3">
    <div className="grid grid-cols-2 gap-4">{controls.map(setting => <SettingControl key={setting.id} setting={setting} group={group} settings={protocol.settings} onChange={onChange} onError={onError} />)}</div>
  </div>;
}
