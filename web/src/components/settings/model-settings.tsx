"use client";
import { useId, useState } from "react";
import type { ModelGroupConfig } from "@/lib/config-types";
import type { ModelSettingInfo } from "@/lib/provider-admin";
import { applicable, modelSettingErrors, object, overrideWarnings, parseCustomPrimitive, pointerKey, removePointer, schemaError, type ProtocolInfo, type Schema } from "@/lib/model-authoring";
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

export function CustomParameters({ group, protocol, onChange, onError }: { group: ModelGroupConfig; protocol: ProtocolInfo;
  onChange: (group: ModelGroupConfig) => void; onError: (path: string, error: string | null) => void }) {
  const [key, setKey] = useState("");
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const values = group.extra_params ?? {};
  function update(next: Record<string, unknown>) { onChange({ ...group, extra_params: next }); }
  function fail(message: string | null) { setError(message); onError("/extra_params/new", message); }
  function add() {
    try {
      if (!key.trim()) throw new Error("Enter a custom parameter key");
      if (Object.hasOwn(values, key)) throw new Error("That key already exists; edit its row instead");
      const value = parseCustomPrimitive(text);
      const next = { ...values, [key]: value };
      const errors = modelSettingErrors({ ...group, extra_params: next }, protocol).filter(error => error.startsWith("/extra_params"));
      if (errors.length) throw new Error(errors.join("; "));
      update(next); setKey(""); setText(""); fail(null);
    } catch (error) { fail(error instanceof Error ? error.message : "Invalid custom parameter"); }
  }
  return <div className="space-y-3">
    <p className="text-xs">Keys are literal. Values accept JSON primitives; quoted JSON strings force string interpretation. Unknown keys are unverified.</p>
    {Object.entries(values).map(([key, value]) => {
      const path = `/extra_params/${pointerKey(key)}`;
      const described = protocol.settings.find(setting => setting.catalog_path && setting.storage?.config_path === path);
      if (described) return null;
      return <div key={key} className="flex items-end gap-2">
        {object(value) || Array.isArray(value) ? <div className="min-w-0 flex-1"><p className="text-sm">{key}</p><pre className="overflow-auto text-xs">{JSON.stringify(value)}</pre><p className="text-xs">Complex value preserved. Edit its internals in YAML.</p></div>
          : <div className="flex-1"><ParsedInput label={`Custom ${key}`} value={value} parse={parseCustomPrimitive}
            onCommit={value => update({ ...values, [key]: value })} onError={error => onError(path, error)} /></div>}
        <button className={button} onClick={() => { const next = { ...values }; delete next[key]; update(next); onError(path, null); }}>Delete {key}</button>
      </div>;
    })}
    <div className="grid grid-cols-2 gap-2"><label className="text-sm">Custom key<input className={`${input} block w-full`} value={key} onChange={event => { setKey(event.target.value); fail(null); }} /></label>
      <label className="text-sm">Custom value<input className={`${input} block w-full`} value={text} onChange={event => { setText(event.target.value); fail(null); }} /></label></div>
    <div className="flex gap-2"><button className={button} onClick={add}>Add custom parameter</button>
      {Object.keys(values).length > 0 && <button className={button} onClick={() => { update({}); fail(null); for (const key of Object.keys(values)) onError(`/extra_params/${pointerKey(key)}`, null); }}>Clear custom parameters</button>}</div>
    {error && <p role="alert" className="text-sm text-error-text">{error}</p>}
    {overrideWarnings(group, protocol).map(warning => <p key={warning} className="text-sm text-warning">{warning}</p>)}
  </div>;
}

export function ModelSettings({ group, protocol, onChange, onError }: { group: ModelGroupConfig; protocol: ProtocolInfo;
  onChange: (group: ModelGroupConfig) => void; onError: (path: string, error: string | null) => void }) {
  const controls = protocol.settings.filter(setting => setting.storage?.config_path !== "/extra_params" && setting.support !== "configured_unverified");
  return <div className="space-y-3">
    <div className="grid grid-cols-2 gap-4">{controls.map(setting => <SettingControl key={setting.id} setting={setting} group={group} settings={protocol.settings} onChange={onChange} onError={onError} />)}</div>
  </div>;
}
