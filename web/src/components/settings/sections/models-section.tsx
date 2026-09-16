"use client";

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { getConfigSchema, type ModelGroupConfig, type ModelProviderConfig } from "@/lib/config-types";
import type { ModelDirectory, ModelSettingInfo, ProviderProtocol } from "@/lib/provider-admin";
import type { ProviderDrafts } from "@/lib/provider-drafts";
import { modelSettingErrors, object, pointerKey, reconcileModelSettings, removePointer, resolveSchema, unsupportedPaths } from "@/lib/model-authoring";
import { useModelDirectories } from "@/lib/use-model-directories";
import { formatGroupName } from "@/lib/model-groups";
import { SectionHeader } from "@/components/settings/field";
import { ComboboxInput } from "@/components/settings/combobox";
import { DeleteConfirmDialog } from "@/components/nav/delete-confirm-dialog";
import { CubeIcon, Cog6ToothIcon, ChevronDownIcon, PlusIcon, XMarkIcon } from "@heroicons/react/24/outline";
import { ModelSelector } from "@/components/settings/model-selector";
import { ModelSettings, SettingControl } from "@/components/settings/model-settings";

const EMPTY_PROVIDERS: Record<string, ModelProviderConfig> = {};
const EMPTY_DRAFTS: ProviderDrafts = {};
const button = "rounded-lg border border-border px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-surface-tertiary transition disabled:opacity-50";

interface ModelsSectionProps {
  models: Record<string, ModelGroupConfig>;
  enabledProviders: string[];
  providerConfigs?: Record<string, ModelProviderConfig>;
  savedProviderConfigs?: Record<string, ModelProviderConfig>;
  providerDrafts?: ProviderDrafts;
  savedModels?: Record<string, ModelGroupConfig>;
  onChange: (models: Record<string, ModelGroupConfig>, removedGroups?: string[]) => void;
  onReadyChange?: (blockReason: string | null) => void;
}

const PREDEFINED_GROUPS = ["primary", "coding", "reasoning", "memory"];
const OPTIONAL_GROUPS = ["coding", "reasoning", "memory"];

function sortedGroupNames(names: string[]): string[] {
  const predefined = PREDEFINED_GROUPS.filter((g) => names.includes(g));
  const custom = names.filter((g) => !PREDEFINED_GROUPS.includes(g));
  return [...predefined, ...custom];
}

interface GroupNameInputProps {
  value: string;
  suggestions: string[];
  onRename: (newName: string) => void;
}

function GroupNameInput({ value, suggestions, onRename }: GroupNameInputProps) {
  const displayDraft = formatGroupName(value);
  const [draft, setDraft] = useState(displayDraft);

  const nameToId = new Map(suggestions.map((g) => [formatGroupName(g), g]));
  const items = suggestions.map((g) => {
    const display = formatGroupName(g);
    return { value: display, label: display };
  });

  return (
    <ComboboxInput
      label="Group ID"
      value={draft}
      items={items}
      placeholder="e.g. Primary, Coding"
      allowFreeText
      onChange={(v) => {
        setDraft(v);
        const id = nameToId.get(v);
        if (id) {
          onRename(id);
        }
      }}
      onBlur={() => {
        const resolved = nameToId.get(draft) ?? draft;
        const sanitized = resolved.trim().toLowerCase().replace(/\s+/g, "_").replace(/[^a-z0-9_]/g, "");
        if (sanitized && sanitized !== value) {
          setDraft(formatGroupName(sanitized));
          onRename(sanitized);
        } else {
          setDraft(formatGroupName(value));
        }
      }}
    />
  );
}

function CollapsibleSection({ title, defaultOpen = false, children }: { title: string; defaultOpen?: boolean; children: React.ReactNode }) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="border-b border-border last:border-b-0">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
        className="flex w-full items-center justify-between py-3 text-sm font-medium text-text-secondary hover:text-text-primary transition"
      >
        <span>{title}</span>
        <svg
          className={`h-4 w-4 text-text-tertiary transition-transform ${open ? "rotate-90" : ""}`}
          fill="none"
          viewBox="0 0 24 24"
          stroke="currentColor"
          strokeWidth={2}
        >
          <path strokeLinecap="round" strokeLinejoin="round" d="M9 5l7 7-7 7" />
        </svg>
      </button>
      {open && <div className="pb-4 space-y-4">{children}</div>}
    </div>
  );
}

function ParametersDialog({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);
  return <div className="fixed inset-0 z-50 flex items-center justify-center" role="dialog" aria-modal="true" aria-label={`${title} parameters`}>
    <div className="absolute inset-0 bg-black/50" onClick={onClose} />
    <div className="relative mx-4 flex max-h-[85vh] w-full max-w-lg flex-col rounded-xl border border-border bg-surface-secondary p-4 shadow-xl">
      <div className="-mx-4 flex items-start justify-between gap-3 border-b border-border px-4 pb-3">
        <div><h3 className="text-lg font-semibold text-text-primary">{title}</h3><p className="mt-1 text-sm text-text-tertiary">Model parameters</p></div>
        <button type="button" aria-label="Close parameters" onClick={onClose} className="shrink-0 rounded-lg p-1.5 text-text-tertiary hover:bg-surface-tertiary hover:text-text-primary"><XMarkIcon className="h-4 w-4" /></button>
      </div>
      <div className="mt-1 overflow-y-auto">{children}</div>
      <div className="-mx-4 mt-2 flex justify-center border-t border-border px-4 pt-3">
        <button type="button" onClick={onClose} className="rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover">Close</button>
      </div>
    </div>
  </div>;
}

const protocolLabel = (api: string) => ({ completions: "Chat Completions", responses: "Responses", "anthropic-messages": "Anthropic Messages", "gemini-generate-content": "Gemini Generate Content", ollama: "Ollama" })[api] ?? formatGroupName(api.replaceAll("-", "_"));

function ModelEditor({ group, enabledProviders, configs, directory, loading, error, onChange, report, requireApi, title, onRemove, retrySettings, schemaError, onRefresh }: {
  group: ModelGroupConfig; enabledProviders: string[]; configs: Record<string, ModelProviderConfig>;
  directory?: ModelDirectory; loading?: boolean; error?: string;
  onChange: (group: ModelGroupConfig) => void; report: (message: string | null) => void;
  requireApi?: boolean; title: string; onRemove?: () => void;
  retrySettings: ModelSettingInfo[]; schemaError: string | null; onRefresh: () => void;
}) {
  const [paramsOpen, setParamsOpen] = useState(false);
  const initialNew = useRef(!group.provider || !group.model);
  const [selectionChanged, setSelectionChanged] = useState(false);
  const [pendingSelection, setPendingSelection] = useState<{ provider: string; model: string } | null>(null);
  const isNew = (requireApi ?? initialNew.current) || selectionChanged;
  const [inputErrors, setInputErrors] = useState<Record<string, string>>({});
  const [editorEpoch, setEditorEpoch] = useState(0);
  const row = directory?.models.find(row => row.id === group.model);
  const protocol = row?.protocols.find(protocol => protocol.api === group.api) ?? (!group.api ? row?.protocols[0] : undefined);
  const available = row?.protocols.filter(protocol => protocol.available) ?? [];
  const soleApi = available.length === 1 ? available[0].api : undefined;
  useEffect(() => {
    if (isNew && !group.api && soleApi) onChange({ ...group, api: soleApi });
  }, [isNew, group, soleApi, onChange]);
  useEffect(() => {
    if (!pendingSelection || pendingSelection.provider !== group.provider || pendingSelection.model !== group.model || loading || error) return;
    const selected = row?.protocols.find(protocol => protocol.available && protocol.api === group.api)
      ?? row?.protocols.find(protocol => protocol.available && protocol.api === row.suggested_protocol)
      ?? row?.protocols.find(protocol => protocol.available);
    if (!selected) return;
    setPendingSelection(null);
    onChange(reconcileModelSettings({ ...group, api: selected.api }, selected));
  }, [pendingSelection, group, row, loading, error, onChange]);
  const unsupported = row ? unsupportedPaths(group, protocol) : [];
  const retryErrors = modelSettingErrors(group, { api: group.api ?? "completions", available: true, settings: retrySettings, warnings: [] }).filter(error => error.startsWith("/retry/"));
  const errors = [...modelSettingErrors(group, protocol), ...retryErrors];
  const block = !group.provider || !group.model ? "Select a connection and model ID"
    : isNew && !group.api ? "Select an explicit protocol for this new model"
      : unsupported.length ? `Reconcile unsupported settings: ${unsupported.join(", ")}`
        : errors[0] ?? Object.values(inputErrors)[0] ?? null;
  useEffect(() => { report(block); return () => report(null); }, [block, report]);
  const onError = useCallback((path: string, error: string | null) => {
    setInputErrors(previous => { const next = { ...previous }; if (error) next[path] = `${path}: ${error}`; else delete next[path]; return next; });
  }, []);
  function selectProvider(provider: string) {
    if (provider === group.provider) return;
    setSelectionChanged(true);
    setPendingSelection(null);
    setInputErrors({});
    setEditorEpoch(epoch => epoch + 1);
    onChange({ ...removePointer(group, "/api"), provider, model: "" });
  }
  function selectModel(model: string) {
    const row = directory?.models.find(row => row.id === model);
    const suggested = row?.protocols.find(protocol => protocol.available && protocol.api === row.suggested_protocol)
      ?? row?.protocols.find(protocol => protocol.available);
    const changed = model !== group.model;
    if (changed) {
      setSelectionChanged(true);
      setInputErrors({});
      setEditorEpoch(epoch => epoch + 1);
    }
    const needsProtocol = !group.api || !row?.protocols.some(protocol => protocol.available && protocol.api === group.api);
    const next = { ...group, model, ...((isNew || changed) && needsProtocol && suggested ? { api: suggested.api } : {}) };
    const selected = row?.protocols.find(protocol => protocol.available && protocol.api === next.api);
    if (changed) setPendingSelection(selected ? null : { provider: group.provider, model });
    onChange(changed && selected ? reconcileModelSettings(next, selected) : next);
  }
  return <div className="space-y-2">
    <div className="flex items-end gap-2">
      <div className="min-w-0 flex-1"><ModelSelector provider={group.provider} model={group.model} enabledProviders={enabledProviders} providerConfigs={configs}
        directory={directory} loading={loading} onProviderChange={selectProvider} onModelChange={selectModel} /></div>
      <button type="button" title="Parameters" aria-label={`${title} parameters`} disabled={!group.provider || !group.model} onClick={() => setParamsOpen(true)}
        className="h-[38px] shrink-0 rounded-lg border border-border bg-surface px-2.5 text-text-tertiary transition hover:bg-surface-tertiary hover:text-text-primary disabled:pointer-events-none disabled:opacity-30"><Cog6ToothIcon className="h-4 w-4" /></button>
      {onRemove && <button type="button" aria-label={`Remove ${title}`} onClick={onRemove} className="flex h-[38px] shrink-0 items-center rounded-lg p-1.5 text-text-tertiary hover:bg-surface-tertiary hover:text-text-primary"><XMarkIcon className="h-4 w-4" /></button>}
    </div>
    {error && <p role="alert" className="text-xs text-warning">{error}</p>}
    {!paramsOpen && block && group.provider && group.model && <p className="text-xs text-warning">{block}</p>}
    {paramsOpen && <ParametersDialog title={title} onClose={() => setParamsOpen(false)}>
      <CollapsibleSection title="General" defaultOpen>
        {available.length > 0 && <ComboboxInput label="Protocol" value={group.api ?? ""} allowFreeText={false}
          items={[{ value: "", label: "Use provider default" }, ...available.map(protocol => ({ value: protocol.api, label: protocolLabel(protocol.api) })),
            ...(group.api && !available.some(protocol => protocol.api === group.api) ? [{ value: group.api, label: `${protocolLabel(group.api)} (unavailable)` }] : [])]}
          onChange={api => { if (api) onChange({ ...group, api: api as ProviderProtocol }); else onChange(removePointer(group, "/api")); }} />}
        {row?.warnings.map(warning => <p className="text-xs text-warning" key={warning}>{warning}</p>)}
        {protocol?.warnings.map(warning => <p className="text-xs text-warning" key={warning}>{warning}</p>)}
        {!row && group.model && <p className="text-xs text-text-tertiary">No description is available for this model. Existing settings are preserved.</p>}
        {unsupported.map(path => <p key={path} className="text-sm text-warning">{path} is unsupported by this protocol. {path !== "/api" &&
          <button className={button} onClick={() => onChange(removePointer(group, path))}>Clear {path}</button>}</p>)}
        {protocol && <ModelSettings key={editorEpoch} group={group} protocol={protocol} onChange={onChange} onError={onError} />}
        <button className={button} disabled={!group.provider || loading} onClick={onRefresh}>Refresh descriptions</button>
      </CollapsibleSection>
      <CollapsibleSection title="Retry">
        {schemaError && <p className="text-sm text-warning">{schemaError}. Existing retry values are preserved.</p>}
        <div className="grid grid-cols-2 gap-4">{retrySettings.map(setting => <SettingControl key={setting.id} setting={setting} group={group} settings={retrySettings} onChange={onChange} onError={onError} />)}</div>
      </CollapsibleSection>
      {Object.keys(inputErrors).length > 0 && <button className={button} onClick={() => { setInputErrors({}); setEditorEpoch(epoch => epoch + 1); }}>Discard uncommitted field edits</button>}
      {[...new Set(errors.concat(Object.values(inputErrors)))].map(error => <p role="alert" className="text-sm text-error-text" key={error}>{error}</p>)}
    </ParametersDialog>}
  </div>;
}

function Entry({ id, report, ...props }: Omit<Parameters<typeof ModelEditor>[0], "report"> & {
  id: string; report: (id: string, message: string | null) => void;
}) {
  const reportEntry = useCallback((message: string | null) => report(id, message), [id, report]);
  return <ModelEditor {...props} report={reportEntry} />;
}

export function ModelsSection({ models, enabledProviders, providerConfigs = EMPTY_PROVIDERS, savedProviderConfigs, savedModels,
  providerDrafts = EMPTY_DRAFTS, onChange, onReadyChange }: ModelsSectionProps) {
  const soleProvider = enabledProviders.length === 1 ? enabledProviders[0] : undefined;
  useEffect(() => {
    if (!soleProvider) return;
    function selectProvider(group: ModelGroupConfig): ModelGroupConfig {
      let next = group.provider ? group : { ...group, provider: soleProvider! };
      const fallbacks = group.fallbacks?.map(selectProvider);
      if (fallbacks?.some((fallback, index) => fallback !== group.fallbacks![index])) next = { ...next, fallbacks };
      return next;
    }
    const next = Object.fromEntries(Object.entries(models).map(([name, group]) => [name, selectProvider(group)]));
    if (!next.primary) next.primary = { provider: soleProvider, model: "" };
    if (Object.entries(next).some(([name, group]) => group !== models[name])) onChange(next);
  }, [soleProvider, models, onChange]);
  const { directories, errors, loading, load } = useModelDirectories(models, providerConfigs, providerDrafts, savedProviderConfigs);
  const [reports, setReports] = useState<Record<string, string>>({});
  const [retrySettings, setRetrySettings] = useState<ModelSettingInfo[]>([]);
  const [schemaError, setSchemaError] = useState<string | null>(null);
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set(["primary"]));
  const [confirmingRemove, setConfirmingRemove] = useState<string | null>(null);
  const report = useCallback((id: string, message: string | null) => {
    setReports(previous => {
      if (previous[id] === message || !message && !Object.hasOwn(previous, id)) return previous;
      const next = { ...previous }; if (message) next[id] = message; else delete next[id]; return next;
    });
  }, []);
  useEffect(() => {
    let alive = true;
    getConfigSchema().then(root => {
      const schema = resolveSchema(root, root.$defs?.RetryConfig ?? root.definitions?.RetryConfig);
      if (!object(schema.properties)) throw new Error("Retry schema is unavailable");
      const settings: ModelSettingInfo[] = Object.entries(schema.properties).map(([key, value]) => ({
        id: `retry.${key}`, label: key.replaceAll("_", " "), description: null, group: "retry", scope: "frona_runtime",
        storage: { kind: "typed", config_path: `/retry/${pointerKey(key)}` }, request_path: null, catalog_path: null,
        schema: resolveSchema(root, value), applicability: null, support: "typed", sources: ["Frona config schema"],
      }));
      if (alive) setRetrySettings(settings);
    }).catch(error => { if (alive) setSchemaError(error instanceof Error ? error.message : "Retry schema is unavailable"); });
    return () => { alive = false; };
  }, []);
  const retryBlock = Object.entries(models).flatMap(([name, group]) =>
    modelSettingErrors(group, { api: group.api ?? "completions", available: true, settings: retrySettings, warnings: [] })
      .filter(error => error.startsWith("/retry/")).map(error => `${name}${error}`))[0];
  const block = !models.primary?.provider || !models.primary?.model ? "Configure the Primary model group to continue"
    : retryBlock ?? Object.values(reports)[0] ?? null;
  useEffect(() => { onReadyChange?.(block); }, [block, onReadyChange]);
  function update(name: string, group: ModelGroupConfig) { onChange({ ...models, [name]: group }); }
  function editor(group: ModelGroupConfig, id: string, change: (group: ModelGroupConfig) => void, onRemove?: () => void) {
    const [name, , index] = id.split("/");
    const saved = index === undefined ? savedModels?.[name] : savedModels?.[name]?.fallbacks?.[Number(index)];
    return <Entry key={id} id={id} group={group} enabledProviders={enabledProviders} configs={providerConfigs}
      title={index === undefined ? formatGroupName(name) : `${formatGroupName(name)} fallback ${Number(index) + 1}`} onRemove={onRemove}
      retrySettings={retrySettings} schemaError={schemaError} onRefresh={() => void load(group.provider, [group.model], true).catch(() => {})}
      requireApi={savedModels ? !saved : undefined}
      directory={directories[group.provider]} loading={loading[group.provider]} error={errors[group.provider]} onChange={change} report={report} />;
  }
  function toggleExpanded(name: string) {
    setExpandedGroups(previous => { const next = new Set(previous); if (next.has(name)) next.delete(name); else next.add(name); return next; });
  }
  function removeGroup(name: string) {
    const next = { ...models }; delete next[name]; onChange(next, [name]); setConfirmingRemove(null);
  }
  function renameGroup(oldName: string, newName: string) {
    if (!/^[a-z][a-z0-9_]*$/.test(newName) || newName === oldName || newName in models) return;
    onChange(Object.fromEntries(Object.entries(models).map(([name, group]) => [name === oldName ? newName : name, group])), [oldName]);
    setExpandedGroups(previous => { const next = new Set(previous); if (next.delete(oldName)) next.add(newName); return next; });
  }
  function enableGroup(name: string) {
    update(name, { provider: "", model: "", fallbacks: [] });
    setExpandedGroups(previous => new Set(previous).add(name));
  }
  function addGroup() {
    let name = "custom_group";
    for (let index = 1; name in models; index++) name = `custom_group_${index}`;
    enableGroup(name);
  }
  const names = sortedGroupNames([...new Set(["primary", ...Object.keys(models)])]);
  const availableGroups = OPTIONAL_GROUPS.filter(name => !(name in models));
  return <div>
    <SectionHeader title="Model Groups" description="Configure model groups with fallback chains and inference parameters" icon={CubeIcon} />
    <div className="space-y-3">
      {names.map(name => {
        const group = models[name] ?? { provider: "", model: "" };
        const expanded = expandedGroups.has(name);
        const primary = name === "primary";
        return <div key={name} role="group" aria-label={`Model group ${name}`} className="rounded-lg border border-border bg-surface-secondary">
          <div className="flex items-center px-4 py-3">
            <button type="button" aria-expanded={expanded} onClick={() => toggleExpanded(name)} className="flex min-w-0 flex-1 items-center justify-between text-sm font-medium text-text-primary">
              <span className="flex items-center gap-2">{formatGroupName(name)}{primary && <span className="rounded-full bg-accent/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wide text-accent">Required</span>}</span>
              <ChevronDownIcon className={`h-4 w-4 text-text-tertiary transition-transform ${expanded ? "rotate-180" : ""}`} />
            </button>
            {!primary && <button type="button" role="switch" aria-checked="true" aria-label={`Disable ${formatGroupName(name)} model group`}
              onClick={() => { if (PREDEFINED_GROUPS.includes(name)) removeGroup(name); else setConfirmingRemove(name); }}
              className="relative ml-3 inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent bg-accent transition-colors">
              <span className="pointer-events-none inline-block h-5 w-5 translate-x-5 rounded-full bg-surface shadow transition-transform" />
            </button>}
          </div>
          <div hidden={!expanded} className="space-y-4 px-4 pb-4">
            {!PREDEFINED_GROUPS.includes(name) && <GroupNameInput value={name} suggestions={PREDEFINED_GROUPS.filter(id => !(id in models))} onRename={next => renameGroup(name, next)} />}
            {editor(group, name, next => update(name, next))}
            {!!group.fallbacks?.length && <div className="space-y-2"><p className="text-sm font-medium text-text-secondary">Fallbacks</p>
              {group.fallbacks.map((fallback, index) => <div key={index} role="group" aria-label={`Fallback ${index + 1}`} className="flex items-end gap-2">
                <span className="w-5 shrink-0 pb-2.5 text-right text-xs text-text-tertiary">{index + 1}.</span>
                <div className="min-w-0 flex-1">{editor(fallback, `${name}/fallbacks/${index}`, next => update(name, { ...group, fallbacks: group.fallbacks!.map((value, i) => i === index ? next : value) }),
                  () => update(name, { ...group, fallbacks: group.fallbacks!.filter((_, i) => i !== index) }))}</div>
              </div>)}
            </div>}
            <div className="flex items-center gap-2 pt-1"><button type="button" className={`${button} flex items-center gap-1.5`} aria-label="Add fallback"
              onClick={() => update(name, { ...group, fallbacks: [...group.fallbacks ?? [], { provider: "", model: "" }] })}><PlusIcon className="h-3.5 w-3.5" />Fallback</button></div>
          </div>
        </div>;
      })}
    </div>
    {!!availableGroups.length && <div className="mt-4 space-y-2">{availableGroups.map(name => <div key={name} className="flex items-center justify-between rounded-lg border border-border bg-surface-secondary px-4 py-3">
      <div className="flex items-center gap-2"><span className="text-sm text-text-secondary">{formatGroupName(name)}</span><span className="rounded-full bg-surface-tertiary px-2 py-0.5 text-[10px] font-medium text-text-tertiary">Uses Primary</span></div>
      <button type="button" aria-label={`Enable ${formatGroupName(name)} model group`} onClick={() => enableGroup(name)} className="rounded-lg bg-surface-tertiary px-3 py-1 text-xs font-medium text-text-secondary transition hover:bg-accent hover:text-surface">Enable</button>
    </div>)}</div>}
    <button type="button" onClick={addGroup} className="mt-4 rounded-lg bg-accent px-4 py-2 text-sm text-surface hover:bg-accent-hover">+ Add Model Group</button>
    <DeleteConfirmDialog open={!!confirmingRemove} onCancel={() => setConfirmingRemove(null)} onConfirm={() => { if (confirmingRemove) removeGroup(confirmingRemove); }}
      title={`Remove ${confirmingRemove ? formatGroupName(confirmingRemove) : ""}?`} message="This model group and its configuration will be removed." />
  </div>;
}
