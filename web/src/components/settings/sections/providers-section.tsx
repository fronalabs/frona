"use client";

import { useCallback, useEffect, useEffectEvent, useRef, useState, type Dispatch, type SetStateAction } from "react";
import type { ConfigUpdateResponse, ModelProviderConfig } from "@/lib/config-types";
import { getProviderCatalog, providerAdmin, type CredentialInput, type CredentialMutation, type LoginAttempt,
  type ProviderCatalog, type ProviderCatalogEntry, type ProviderFieldInfo, type ProviderInspection, type ProviderValidation } from "@/lib/provider-admin";
import { matchingDraft, normalizeProviderHandle, prepareProviderDraft, providerFingerprint, readPointer, writePointer,
  type PreparedProviderDraft, type ProviderDrafts } from "@/lib/provider-drafts";
import { SectionHeader, SectionPanel, TextInput, Toggle } from "@/components/settings/field";
import { ComboboxInput } from "@/components/settings/combobox";
import { CopyButton } from "@/components/ui/copy-button";
import { CloudIcon, InformationCircleIcon, XMarkIcon } from "@heroicons/react/24/outline";
import Image from "next/image";

const SAVED_CREDENTIAL_METHOD = "saved_credential";
const LOGIN_POLL_INTERVAL_MS = 10_000;

const button = "rounded-lg border border-border bg-surface px-3 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary transition-colors disabled:cursor-not-allowed disabled:opacity-50";
export type TestStatus = "idle" | "testing" | "success" | "error";
export function TestStatusIcon({ status }: { status: TestStatus }) {
  if (status === "testing") {
    return (
      <svg className="h-4 w-4 animate-spin text-text-tertiary" viewBox="0 0 24 24" fill="none">
        <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
        <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
      </svg>
    );
  }
  if (status === "success") {
    return (
      <svg className="h-4 w-4 text-green-500" viewBox="0 0 20 20" fill="currentColor">
        <path fillRule="evenodd" d="M10 18a8 8 0 100-16 8 8 0 000 16zm3.707-9.293a1 1 0 00-1.414-1.414L9 10.586 7.707 9.293a1 1 0 00-1.414 1.414l2 2a1 1 0 001.414 0l4-4z" clipRule="evenodd" />
      </svg>
    );
  }
  if (status === "error") {
    return (
      <svg className="h-4 w-4 text-red-500" viewBox="0 0 20 20" fill="currentColor">
        <path fillRule="evenodd" d="M10 18a8 8 0 100-16 8 8 0 000 16zM8.707 7.293a1 1 0 00-1.414 1.414L8.586 10l-1.293 1.293a1 1 0 101.414 1.414L10 11.414l1.293 1.293a1 1 0 001.414-1.414L11.414 10l1.293-1.293a1 1 0 00-1.414-1.414L10 8.586 8.707 7.293z" clipRule="evenodd" />
      </svg>
    );
  }
  return null;
}
function message(error: unknown): string { return error instanceof Error ? error.message : "Provider request failed"; }
function safeLink(value: string): string | undefined {
  try { const url = new URL(value); return ["http:", "https:"].includes(url.protocol) && !url.username && !url.password ? url.href : undefined; }
  catch { return undefined; }
}

function optionLabel(value: string): string {
  const labels: Record<string, string> = {
    api_key: "API key", "api-key": "API key", oauth: "Sign in", chatgpt_oauth: "ChatGPT subscription",
    openrouter_connect: "Connect OpenRouter", azure_entra: "Microsoft Entra ID", aws: "AWS credentials",
    anonymous: "No authentication", static: "API key", openai_codex: "ChatGPT", github_copilot: "GitHub Copilot",
    database: "Saved credential", config: "Configuration", ambient: "Server credentials", none: "Not connected",
    api: "API access", subscription: "Subscription", client_credentials: "Client credentials",
    managed_identity: "Managed identity", default: "Default", unavailable: "Unavailable",
    live: "Provider account", catalog_fallback: "Catalog", configured_only: "Configured models", live_error: "Provider unavailable",
    unsupported_protocol_for_auth_method: "This authentication method does not support the selected protocol",
  };
  return labels[value] ?? (/^[a-z0-9_-]+$/.test(value) ? value.replace(/[_-]+/g, " ").replace(/^./, character => character.toUpperCase()) : value);
}

function authenticationSourceLabel(source?: string): string {
  if (!source) return "Not connected";
  if (source.startsWith("environment:")) return `Environment variable (${source.slice(12)})`;
  return optionLabel(source);
}

function Field({ field, value, disabled, placeholder, onChange }: { field: ProviderFieldInfo; value: unknown; disabled: boolean; placeholder?: string; onChange: (value: unknown) => void }) {
  const options = Array.isArray(field.schema.enum) ? field.schema.enum : undefined;
  const label = `${field.label}${field.required ? " *" : ""}`;
  return <div className="space-y-1">
    {field.schema.type === "boolean" ? <Toggle label={label} value={value !== false} disabled={disabled} onChange={onChange} />
      : options ? <ComboboxInput label={label} value={value == null ? "" : String(value)} disabled={disabled} allowFreeText={false}
        items={[{ value: "", label: "Use default" }, ...options.map(option => ({ value: String(option), label: optionLabel(String(option)) }))]}
        onChange={selected => onChange(selected === "" ? null : options.find(option => String(option) === selected))} />
      : <TextInput label={label} placeholder={placeholder} type={field.sensitive ? "password" : ["integer", "number"].includes(String(field.schema.type)) ? "number" : "text"}
        autoComplete={field.sensitive ? "new-password" : "off"} value={typeof value === "string" || typeof value === "number" ? String(value) : ""}
        required={field.required} disabled={disabled} onChange={value => onChange(value === "" ? null : ["integer", "number"].includes(String(field.schema.type)) ? Number(value) : value)} />}
  </div>;
}

export function LoginChallenge({ attempt }: { attempt: LoginAttempt }) {
  if (!attempt.challenge) return <p role="status" className="text-sm text-text-secondary">Login {attempt.status}</p>;
  const url = safeLink(attempt.challenge.url);
  return <div className="flex items-start gap-3 rounded-lg border border-warning/30 bg-warning/5 p-4 text-sm text-text-secondary" role="status">
    <InformationCircleIcon className="mt-0.5 h-5 w-5 shrink-0 text-warning" aria-hidden="true" />
    <div className="min-w-0 space-y-2">
      <p>{attempt.challenge.message ?? "Complete login with the provider, then check its status here."}</p>
      {attempt.challenge.user_code && <div className="flex items-center gap-2">
        <p>{attempt.challenge.kind === "device_code" ? "Device code" : "Login code"}: <code>{attempt.challenge.user_code}</code></p>
        <CopyButton value={attempt.challenge.user_code} className="shrink-0" />
      </div>}
      {url && <a className="inline-block text-accent underline" href={url} target="_blank" rel="noreferrer noopener">Open provider login</a>}
    </div>
  </div>;
}

interface CardProps {
  handle: string; config: ModelProviderConfig; entry: ProviderCatalogEntry; inspection?: ProviderInspection;
  draft?: PreparedProviderDraft; revision: string; hasUnsavedChanges: boolean;
  onChange: (config: ModelProviderConfig) => void; onRename: (handle: string) => boolean; onRemove: () => void;
  onDraft: (draft?: PreparedProviderDraft) => void; onSaved: (result: ConfigUpdateResponse) => void;
  onRefresh: () => Promise<void>; report: (handle: string, reason: string | null) => void;
}

function ConnectionCard({ handle, config, entry, inspection, draft, revision, hasUnsavedChanges, onChange, onRename,
  onRemove, onDraft, onSaved, onRefresh, report }: CardProps) {
  const [handleText, setHandleText] = useState(handle);
  const [methodId, setMethodId] = useState<string | null>(null);
  const [credentials, setCredentials] = useState<Record<string, unknown>>({});
  const environmentSource = (typeof config.api_key === "string" ? config.api_key.match(/^\$\{([^}]+)\}$/)?.[1] : undefined)
    ?? (inspection?.effective_authentication?.source.startsWith("environment:") ? inspection.effective_authentication.source.slice(12) : "");
  const [keySourceOverride, setKeySource] = useState<string | null>(environmentSource ? "environment" : inspection || config.credential_id || draft ? "new" : null);
  const [variableOverride, setVariable] = useState<string | null>(environmentSource || null);
  const [busy, setBusy] = useState(false);
  const [validationStatus, setValidationStatus] = useState<TestStatus>("idle");
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [attempt, setAttempt] = useState<LoginAttempt | null>(null);
  const [loginCode, setLoginCode] = useState("");
  const [savedCredentials, setSavedCredentials] = useState<import("@/lib/provider-admin").SavedCredential[]>([]);
  const [credentialLoadError, setCredentialLoadError] = useState<string | null>(null);
  const [credentialReload, setCredentialReload] = useState(0);
  useEffect(() => {
    let cancelled = false;
    providerAdmin.savedCredentials().then(records => {
      if (!cancelled) { setSavedCredentials(records); setCredentialLoadError(null); }
    }).catch(error => { if (!cancelled) setCredentialLoadError(message(error)); });
    return () => { cancelled = true; };
  }, [revision, config.credential_id, credentialReload]);
  const compatibleCredentials = savedCredentials.filter(record =>
    record.integration === "static" ? entry.auth_methods.some(method => method.credential_method === "api_key")
    : record.integration === "openai_codex" ? entry.id === "openai" && entry.auth_methods.some(method => method.credential_method === "oauth")
    : record.integration === "copilot" && entry.id === "github-copilot" && entry.auth_methods.some(method => method.credential_method === "oauth")
  );
  const hasSavedCredentials = compatibleCredentials.length > 0;
  const configuredCredential = compatibleCredentials.some(record => record.credential_id === config.credential_id);
  const activeMethodId = methodId === SAVED_CREDENTIAL_METHOD && !hasSavedCredentials
    ? entry.auth_methods[0]?.id ?? ""
    : methodId ?? (configuredCredential ? SAVED_CREDENTIAL_METHOD : entry.auth_methods[0]?.id ?? "");
  const usingSavedCredential = activeMethodId === SAVED_CREDENTIAL_METHOD;
  const selected = usingSavedCredential ? undefined : entry.auth_methods.find(method => method.id === activeMethodId) ?? entry.auth_methods[0];
  const [environmentVariables, setEnvironmentVariables] = useState<string[]>([]);
  const [environmentLoadError, setEnvironmentLoadError] = useState<string | null>(null);
  const [environmentReload, setEnvironmentReload] = useState(0);
  const suggestedVariable = selected?.fields.filter(field => field.target.kind === "credential")
    .flatMap(field => field.suggested_env).find(name => environmentVariables.includes(name));
  const keySource = keySourceOverride ?? (suggestedVariable ? "environment" : "new");
  const variable = variableOverride ?? suggestedVariable ?? "";
  useEffect(() => {
    if (selected?.interaction !== "form") return;
    let cancelled = false;
    providerAdmin.environmentVariables().then(names => {
      if (!cancelled) { setEnvironmentVariables(names); setEnvironmentLoadError(null); }
    }).catch(error => { if (!cancelled) setEnvironmentLoadError(message(error)); });
    return () => { cancelled = true; };
  }, [selected?.interaction, environmentReload]);
  const epoch = useRef(0);
  const loginStatusInFlight = useRef(false);
  useEffect(() => () => { epoch.current += 1; }, []);
  const proof = matchingDraft(config, draft);
  const saved = !!inspection;
  const changed = !saved || providerFingerprint(config) !== providerFingerprint(inspection.configuration);
  const reason = config.enabled === false && !changed ? null : busy ? `${handle}: validation in progress` : error
    ? `${handle}: ${error}` : changed && !config.credential_id && !proof ? `${handle}: validate the connection before saving` : null;
  useEffect(() => { report(handle, reason); }, [handle, reason, report]);

  function invalidate() {
    epoch.current += 1;
    if (attempt?.status === "pending") void providerAdmin.cancelLogin(handle, attempt.id).catch(() => {});
    onDraft(undefined); setError(null); setNotice(null); setAttempt(null); setLoginCode("");
    setValidationStatus("idle");
  }
  function change(next: ModelProviderConfig) { invalidate(); onChange(next); }
  function candidate(): ModelProviderConfig {
    const next = { ...config, provider: config.provider ?? entry.id };
    // Placeholders are not credentials. Clearing the old source is a draft
    // change, so entering a database key does not switch live inference.
    if (keySource === "environment" || selected?.interaction === "ambient_credentials" || selected?.interaction === "none") next.credential_id = null;
    if (selected?.credential_method === "api_key") next.api_key = selected.interaction === "form" && keySource === "environment" ? "${" + variable.trim() + "}" : null;
    else if (next.api_key && typeof next.api_key === "object") next.api_key = null;
    return next;
  }
  async function accept(result: ProviderValidation, next: ModelProviderConfig, source: string, requestEpoch: number) {
    if (requestEpoch !== epoch.current) { await providerAdmin.discard(handle, result.validation_id); return; }
    const prepared = prepareProviderDraft(next, source, result);
    onChange(next); onDraft(prepared); setCredentials({});
    setNotice(null);
    try {
      await providerAdmin.draftModels(handle, prepared);
    } catch { if (requestEpoch === epoch.current) setNotice("Validated draft. Model listing failed; manual entry remains available."); }
  }
  async function validate() {
    if (!selected) return;
    try { if (normalizeProviderHandle(handleText) !== handle) throw new Error("Apply a unique connection handle before validation"); }
    catch (error) { setError(message(error)); return; }
    setKeySource(keySource); setVariable(variable);
    setValidationStatus("testing");
    setBusy(true); setError(null); setNotice(null);
    const requestEpoch = ++epoch.current;
    const next = candidate();
    let credential: CredentialInput;
    if (selected.interaction === "none") credential = { source: "anonymous" };
    else if (selected.interaction === "ambient_credentials") credential = { source: "ambient", method: selected.credential_method };
    else if (keySource === "environment") credential = { source: "environment", variable: variable.trim() };
    else credential = { source: "api_key", api_key: String(credentials.api_key ?? "") };
    const source = credential.source === "environment" ? `environment:${credential.variable}` : credential.source === "api_key" ? "database" : credential.source;
    try {
      await providerAdmin.inspectDraft(handle, next);
      const result = await providerAdmin.validate(handle, next, credential);
      await accept(result, next, source, requestEpoch);
      if (requestEpoch === epoch.current) setValidationStatus("success");
    } catch (error) { if (requestEpoch === epoch.current) { onDraft(undefined); setError(message(error)); setValidationStatus("error"); } }
    finally { if (requestEpoch === epoch.current) { setCredentials({}); setBusy(false); } }
  }
  async function login(action: "start" | "status" | "cancel" | "complete") {
    if (!selected) return;
    const polling = action === "status";
    if (polling && (attempt?.status !== "pending" || loginStatusInFlight.current)) return;
    if (polling && handleText.trim().toLowerCase() !== handle) return;
    try { if (normalizeProviderHandle(handleText) !== handle) throw new Error("Apply a unique connection handle before login"); }
    catch (error) { setError(message(error)); return; }
    if (polling) loginStatusInFlight.current = true;
    else { setBusy(true); setError(null); setCredentials({}); }
    const requestEpoch = polling ? epoch.current : ++epoch.current;
    const next = candidate();
    try {
      const result = action === "start" ? await providerAdmin.startLogin(handle, next, selected.credential_method)
        : action === "cancel" ? await providerAdmin.cancelLogin(handle, attempt!.id)
        : action === "complete" ? await providerAdmin.completeLogin(handle, attempt!.id, loginCode) : await providerAdmin.loginStatus(handle, attempt!.id);
      if (requestEpoch !== epoch.current) return;
      setError(null);
      setAttempt(result);
      if (result.status !== "pending") setLoginCode("");
      if (result.status === "validated" && result.credential_id) {
        setValidationStatus("success");
        onChange({...next, api_key: null, credential_id: result.credential_id});
        onDraft(undefined);
        setNotice("Credential saved. Replacements affect every reference immediately. Save the configuration to persist this selection.");
        await onRefresh();
      }
      else if (["failed", "expired", "cancelled"].includes(result.status)) { onDraft(undefined); setError(result.error ?? `Login ${result.status}`); setValidationStatus("error"); }
    } catch (error) { if (requestEpoch === epoch.current) setError(message(error)); }
    finally {
      if (polling) loginStatusInFlight.current = false;
      else if (requestEpoch === epoch.current) setBusy(false);
    }
  }
  const pollLogin = useEffectEvent(async () => { await login("status"); });
  useEffect(() => {
    if (attempt?.status !== "pending" || busy) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      await pollLogin();
      if (!cancelled) timer = setTimeout(poll, LOGIN_POLL_INTERVAL_MS);
    }
    timer = setTimeout(poll, LOGIN_POLL_INTERVAL_MS);
    return () => { cancelled = true; clearTimeout(timer); };
  }, [attempt?.id, attempt?.status, busy]);

  async function mutate(action: () => Promise<CredentialMutation>) {
    setBusy(true); setError(null);
    try {
      const result = await action(); onDraft(undefined); setCredentials({});
      setNotice(`Active credential updated. Affected groups: ${result.affected_groups.join(", ") || "none"}. ${Object.values(result.unavailable_models).flat().map(([model, reason]) => `${model}: ${optionLabel(reason)}`).join("; ")}`);
      await onRefresh();
    } catch (error) { setError(message(error)); }
    finally { setBusy(false); }
  }
  async function remove() {
    if (!saved) {
      epoch.current += 1;
      if (attempt?.status === "pending") void providerAdmin.cancelLogin(handle, attempt.id).catch(() => {});
      if (draft) await providerAdmin.discard(handle, draft.validation_id).catch(() => {});
      onRemove(); return;
    }
    if (!window.confirm(`Delete ${handle}? Active configuration changes after restart.`)) return;
    setBusy(true); setError(null);
    try { onSaved(await providerAdmin.delete(handle, revision)); await onRefresh(); }
    catch (error) { setError(message(error)); }
    finally { setBusy(false); }
  }
  const validationConfig = candidate();
  const validationFingerprint = providerFingerprint(validationConfig);
  const requiredFieldsPresent = [...entry.fields, ...(selected?.fields ?? [])].every(field => {
    if (!field.required) return true;
    const value = field.target.kind === "configuration" ? readPointer(validationConfig, field.target.path)
      : keySource === "environment" ? variable.trim() : credentials[field.target.field];
    return value != null && (typeof value !== "string" || value.trim() !== "");
  });
  const readyToValidate = config.enabled !== false && !!selected?.validation_available
    && selected.interaction !== "backend_login" && requiredFieldsPresent
    && (selected.interaction !== "form" || !!(keySource === "environment" ? variable.trim() : String(credentials.api_key ?? "").trim()))
    && handleText.trim().toLowerCase() === handle && !busy && !proof && !error;
  const autoValidate = useEffectEvent(() => { void validate(); });
  useEffect(() => {
    if (!readyToValidate) return;
    const timer = setTimeout(() => autoValidate(), 800);
    return () => clearTimeout(timer);
  }, [readyToValidate, validationFingerprint, credentials, activeMethodId, keySource, variable, handle, handleText]);
  const displayedStatus = attempt?.status === "pending" || validationStatus === "testing" ? "testing" : proof ? "success" : validationStatus;
  const statusLabel = displayedStatus === "testing" ? "Validating connection" : displayedStatus === "success"
    ? "Connection validated" : displayedStatus === "error" ? "Connection validation failed" : "Not validated";
  const logoUrl = entry.logo_url ? safeLink(entry.logo_url) : undefined;
  const documentationUrl = entry.documentation_url ? safeLink(entry.documentation_url) : undefined;

  return <section aria-label={`Connection ${handle}`}><SectionPanel className={logoUrl ? "px-3! pb-3! pt-0! sm:px-4! sm:pb-4!" : "p-3! sm:p-4!"}>
    <div className={`flex items-center justify-between gap-2 ${logoUrl ? "mb-0!" : ""}`}><h3 className="flex items-center gap-2 font-medium">
      {logoUrl && <Image src={logoUrl} alt="" width={64} height={64} className="h-[clamp(2.5rem,4vw,4rem)] w-auto shrink-0" unoptimized />}
      {documentationUrl ? <a href={documentationUrl} target="_blank" rel="noreferrer noopener" className="hover:text-accent hover:underline">{entry.name}</a> : entry.name}
      <span role="img" aria-label={statusLabel} title={displayedStatus === "error" ? error ?? statusLabel : statusLabel}>
        {displayedStatus === "idle" ? <span className="block h-2 w-2 rounded-full bg-text-tertiary" /> : <TestStatusIcon status={displayedStatus} />}
      </span>
      </h3>
      <button type="button" className="shrink-0 rounded-lg p-2 text-text-tertiary hover:bg-surface-tertiary hover:text-error-text disabled:cursor-not-allowed disabled:opacity-50"
        aria-label={saved ? "Delete connection" : "Remove draft"} disabled={busy || (saved && hasUnsavedChanges)}
        title={saved && hasUnsavedChanges ? "Save or discard pending changes before deletion" : saved ? "Delete connection" : "Remove draft"} onClick={remove}>
        <XMarkIcon className="h-4 w-4" aria-hidden="true" />
      </button>
    </div>
    <TextInput label="Connection handle" value={handleText} disabled={busy} onChange={setHandleText}
      onBlur={() => {
        try {
          const next = normalizeProviderHandle(handleText);
          setHandleText(next);
          if (next !== handle && onRename(next)) {
            epoch.current += 1;
            if (draft) void providerAdmin.discard(handle, draft.validation_id).catch(() => {});
            if (attempt?.status === "pending") void providerAdmin.cancelLogin(handle, attempt.id).catch(() => {});
          }
        } catch (error) { setError(message(error)); }
      }} />

    {inspection && <p className="text-sm text-text-secondary">Active authentication: {authenticationSourceLabel(inspection.effective_authentication?.source)}.</p>}
    {inspection?.affected_groups.length ? <p className="text-sm">Used by: {inspection.affected_groups.join(", ")}</p> : null}
    {inspection?.pending_removal && <p role="status">Removal is pending restart.</p>}
    {entry.warnings.map(warning => <p key={warning} className="text-sm text-warning">{warning}</p>)}
    {entry.fields.map(field => field.target.kind === "configuration" && <Field key={field.id} field={field} value={readPointer(config, field.target.path)} disabled={busy}
      placeholder={field.target.path === "/base_url" ? entry.default_base_url ?? undefined : undefined}
      onChange={value => field.target.kind === "configuration" && change(writePointer(config, field.target.path, value))} />)}
    <ComboboxInput label="Authentication method" value={activeMethodId} disabled={busy} allowFreeText={false}
      items={[...entry.auth_methods.map(method => ({ value: method.id, label: optionLabel(method.id) })),
        ...(hasSavedCredentials ? [{ value: SAVED_CREDENTIAL_METHOD, label: "Saved credential" }] : [])]}
      onChange={method => {
        invalidate(); setCredentials({}); setMethodId(method);
        if (usingSavedCredential && method !== SAVED_CREDENTIAL_METHOD) onChange({ ...config, credential_id: null });
      }} />
    {credentialLoadError && <div className="space-y-2">
      <p role="alert" className="text-sm text-error-text">{credentialLoadError}</p>
      <button className={button} disabled={busy} onClick={() => { setCredentialLoadError(null); setCredentialReload(value => value + 1); }}>Retry saved credentials</button>
    </div>}
    {usingSavedCredential && <>
      <ComboboxInput label="Saved credential" value={config.credential_id ?? ""} disabled={busy} allowFreeText={false}
        placeholder="Choose a credential"
        items={[{ value: "", label: "Choose a credential" }, ...compatibleCredentials.map(record => ({ value: record.credential_id, label: record.name }))]}
        onChange={credential_id => change({...config, api_key: null, aws_profile: null, azure_credential: null, credential_id: credential_id || null})} />
      {config.credential_id && <p className="text-xs text-text-tertiary">Connections using this credential share its authentication.</p>}
    </>}
    {selected?.interaction === "form" && <>
      <ComboboxInput label="Credential source" value={keySource} disabled={busy} allowFreeText={false}
        items={[{ value: "new", label: "Enter a new API key" }, { value: "environment", label: "Server environment variable" }]}
        onChange={source => { invalidate(); setCredentials({}); setKeySource(source); }} />
      {keySource === "environment" ? <div className="space-y-2"><ComboboxInput label="Environment variable" value={variable} disabled={busy} allowFreeText
        items={environmentVariables.map(name => ({ value: name, label: name }))}
        placeholder="Select or enter a variable name"
        onChange={value => { invalidate(); setVariable(value); }} />
        {environmentLoadError && <>
          <p role="alert" className="text-sm text-error-text">{environmentLoadError}</p>
          <button className={button} disabled={busy} onClick={() => { setEnvironmentLoadError(null); setEnvironmentReload(value => value + 1); }}>Retry environment variables</button>
        </>}
      </div>
        : keySource === "new" ? selected.fields.filter(field => field.target.kind === "credential").map(field => <Field key={field.id} field={field} value={field.target.kind === "credential" ? credentials[field.target.field] : undefined}
          placeholder={field.sensitive && (proof?.source === "database" || config.credential_id || (typeof config.api_key === "object" && config.api_key?.is_set)) ? "\u2022".repeat(8) : undefined}
          disabled={busy} onChange={value => { if (field.target.kind === "credential") { const key = field.target.field; invalidate(); setKeySource("new"); setCredentials(previous => ({ ...previous, [key]: value })); }
            else change(writePointer(config, field.target.path, value)); }} />) : <p className="text-sm">The stored credential stays on the server.</p>}
    </>}
    {selected?.interaction === "ambient_credentials" && <p className="text-sm">The server will check its credential chain. Select the account location below.</p>}
    {selected?.fields.map(field => field.target.kind === "configuration" && <Field key={field.id} field={field} value={readPointer(config, field.target.path)} disabled={busy}
      onChange={value => field.target.kind === "configuration" && change(writePointer(config, field.target.path, value))} />)}
    {selected?.interaction === "none" && <p className="text-sm">No credential is required. Validation checks server connectivity.</p>}
    <div className="flex flex-wrap gap-2">
      {selected?.interaction === "backend_login" && <button className={button} disabled={busy || attempt?.status === "pending"} onClick={() => login("start")}>Connect</button>}
      {attempt?.status === "pending" && <button className={button} disabled={busy} onClick={() => login("cancel")}>Cancel login</button>}
      {inspection?.credentials.filter(credential => credential.state === "active").map(credential => <button key={credential.method} className={button} disabled={busy} onClick={() => {
        if (window.confirm(`Log out ${handle}'s ${optionLabel(credential.method)} credential now? This affects active inference.`)) mutate(() => providerAdmin.logout(handle, credential.method, credential.generation));
      }}>Log out {optionLabel(credential.method)}</button>)}
    </div>
    {attempt && <LoginChallenge attempt={attempt} />}
    {attempt?.status === "pending" && attempt.challenge?.kind === "redirect" && <div className="space-y-2">
      <div className="flex items-end gap-2">
        <div className="min-w-0 flex-1"><TextInput label="Login completion code" type="password" autoComplete="off" value={loginCode} onChange={setLoginCode} disabled={busy} /></div>
        {loginCode && <CopyButton value={loginCode} className="mb-1 shrink-0" />}
      </div>
      <button className={button} disabled={busy || !loginCode} onClick={() => login("complete")}>Complete login</button>
    </div>}
    {notice && <p role="status" className="text-sm">{notice}</p>}
    {error && <p role="alert" className="text-sm text-error-text">{error}</p>}
  </SectionPanel></section>;
}

interface ProvidersSectionProps {
  providers: Record<string, ModelProviderConfig>;
  onChange: (providers: Record<string, ModelProviderConfig>, removed?: string[]) => void;
  onReadyChange?: (blockReason: string | null) => void;
  drafts: ProviderDrafts; onDraftsChange: Dispatch<SetStateAction<ProviderDrafts>>;
  persistedRevision: string; hasUnsavedChanges: boolean;
  onSaved: (result: ConfigUpdateResponse) => void;
  requireEnabledProvider?: boolean;
}

export function ProvidersSection({ providers, onChange, onReadyChange, drafts, onDraftsChange, persistedRevision,
  hasUnsavedChanges, onSaved, requireEnabledProvider = false }: ProvidersSectionProps) {
  const [catalog, setCatalog] = useState<ProviderCatalog | null>(null);
  const [inspections, setInspections] = useState<Record<string, ProviderInspection>>({});
  const [selected, setSelected] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [blocks, setBlocks] = useState<Record<string, string | null>>({});
  const refresh = useCallback(async () => {
    const result = await providerAdmin.list();
    setInspections(Object.fromEntries(result.providers.map(provider => [provider.handle, provider])));
  }, []);
  useEffect(() => {
    let cancelled = false;
    Promise.all([getProviderCatalog(), providerAdmin.list()]).then(([catalog, result]) => {
      if (cancelled) return;
      setCatalog(catalog); setInspections(Object.fromEntries(result.providers.map(provider => [provider.handle, provider])));
    }).catch(error => { if (!cancelled) setError(message(error)); });
    return () => { cancelled = true; };
  }, []);
  const report = useCallback((handle: string, reason: string | null) => setBlocks(previous => previous[handle] === reason ? previous : { ...previous, [handle]: reason }), []);
  useEffect(() => {
    if (catalog) refresh().catch(error => setError(message(error)));
  }, [persistedRevision, catalog, refresh]);
  useEffect(() => {
    const enabled = Object.entries(providers).filter(([, config]) => config.enabled !== false);
    const reason = error ?? (!catalog ? "Loading provider metadata..." : requireEnabledProvider && !enabled.length ? "Enable at least one provider to continue"
      : Object.keys(providers).map(handle => blocks[handle]).find(Boolean) ?? null);
    onReadyChange?.(reason);
  }, [providers, blocks, catalog, error, onReadyChange, requireEnabledProvider]);
  function setDraft(handle: string, draft?: PreparedProviderDraft) {
    onDraftsChange(previous => { const next = { ...previous }; if (draft) next[handle] = draft; else delete next[handle]; return next; });
  }
  function add() {
    const entry = catalog?.providers.find(provider => provider.id === selected);
    if (!entry) return;
    try {
      let base = entry.id.trim().toLowerCase().replace(/[^a-z0-9_-]/g, "-");
      if (!/^[a-z]/.test(base) || base.length < 2) base = `provider-${base}`;
      base = base.slice(0, 32);
      let handle = normalizeProviderHandle(base);
      for (let suffix = 2; providers[handle]; suffix++) handle = normalizeProviderHandle(`${base.slice(0, 27)}-${suffix}`);
      onChange({ ...providers, [handle]: { api_key: null, base_url: null, enabled: true, ...entry.configuration_defaults, provider: entry.id } });
      setError(null);
    } catch (error) { setError(message(error)); }
  }
  const staleCatalogSources = Object.entries(catalog?.source_status ?? {})
    .filter(([, status]) => status.state === "stale");
  return <div className="space-y-4">
    <SectionHeader title="Providers" description="Global connections and their credentials" icon={CloudIcon} />
    {error && <p role="alert" className="text-error-text">{error}</p>}
    {error && <button className={button} onClick={async () => {
      try { const next = await getProviderCatalog(); await refresh(); setCatalog(next); setError(null); }
      catch (error) { setError(message(error)); }
    }}>Retry provider metadata</button>}
    {!catalog ? <p role="status">Loading provider metadata...</p> : <>
      {staleCatalogSources.length > 0 && <p className="text-xs text-text-tertiary">Catalog: {staleCatalogSources.map(([name, status]) => `${name} ${status.state}`).join(", ")}</p>}
      <div className="flex items-end gap-2">
        <div className="flex-1">
          <ComboboxInput label="Provider brand" hideLabel value={selected} onChange={setSelected} allowFreeText={false}
            items={catalog.providers.map(provider => ({ value: provider.id, label: provider.name }))}
            placeholder="Choose a provider" />
        </div>
        <button className="rounded-lg border border-transparent bg-accent px-3 py-2 text-sm font-medium text-surface hover:bg-accent-hover transition-colors disabled:cursor-not-allowed disabled:opacity-50"
          disabled={!catalog.providers.some(provider => provider.id === selected)} onClick={add}>Add connection</button>
      </div>
      {Object.entries(providers).map(([handle, config]) => {
        const inspection = inspections[handle];
        const entry = inspection?.setup ?? catalog.providers.find(provider => provider.id === (config.provider ?? handle));
        if (!entry) return <p key={handle} role="alert">{handle}: provider metadata is unavailable. Keep the saved configuration or edit YAML.</p>;
        return <ConnectionCard key={handle} handle={handle} config={config} entry={entry} inspection={inspection} draft={drafts[handle]}
          revision={persistedRevision} hasUnsavedChanges={hasUnsavedChanges} onSaved={onSaved} onRefresh={refresh} report={report}
          onDraft={draft => setDraft(handle, draft)} onChange={config => onChange({ ...providers, [handle]: config })}
          onRemove={() => { const next = { ...providers }; delete next[handle]; setDraft(handle); onChange(next, [handle]); }}
          onRename={nextHandle => { if (providers[nextHandle]) { setError("That connection handle already exists"); return false; }
            const next = { ...providers }; delete next[handle]; next[nextHandle] = config; setDraft(handle); setError(null); onChange(next, [handle]); return true; }} />;
      })}

    </>}
  </div>;
}
