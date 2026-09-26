"use client";

import { useState, useEffect, useId } from "react";
import { api } from "@/lib/api-client";
import { VaultItemPicker, type CredentialOption, type PickerConnection } from "@/components/vault-item-picker";
import type { CredentialTarget, GrantDuration, HitlResponse, ToolCall, VaultField } from "@/lib/types";
import { ApprovalButtons } from "./approval-parts";

function Label({ children }: { children: React.ReactNode }) {
  return <label className="block text-sm font-medium text-text-tertiary mb-1">{children}</label>;
}

export interface ToolContentProps {
  te: ToolCall;
  chatId: string;
  /**
   * Called when the user produces a response. The wizard submits all
   * collected responses in a single batch via the unified resolve endpoint.
   * `displayText` is what we show in the wizard chip for "selected answer".
   */
  onResolve: (response: HitlResponse, displayText: string) => void;
}

export function QuestionContent({ te, onResolve, selectedAnswer }: ToolContentProps & { selectedAnswer?: string }) {
  const hitl = te.hitl;
  if (!hitl || hitl.request.type !== "Question") return null;
  const question = hitl.prompt;
  const options = hitl.request.data.options;

  return (
    <div className="space-y-2">
      <p className="text-sm text-text-primary">{question}</p>
      {options.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {options.map((option) => (
            <button
              key={option}
              onClick={() => onResolve({ type: "Choice", data: option }, option)}
              className={`rounded-lg border px-2.5 py-1 text-xs font-medium transition ${
                selectedAnswer === option
                  ? "border-accent bg-accent/10 text-accent"
                  : "border-border text-text-secondary hover:border-accent hover:text-accent"
              }`}
            >
              {option}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export function TakeoverContent({ te, onResolve }: ToolContentProps) {
  const hitl = te.hitl;
  if (!hitl || hitl.request.type !== "Takeover") return null;
  const { reason, debugger_url } = hitl.request.data;

  return (
    <div className="space-y-2">
      <p className="text-sm text-text-primary">{reason}</p>
      <div className="flex flex-wrap gap-1.5">
        {debugger_url && (
          <a
            href={debugger_url}
            target="_blank"
            rel="noopener noreferrer"
            className="rounded-lg border border-border px-2.5 py-1 text-xs font-medium text-text-secondary hover:border-accent hover:text-accent transition"
          >
            Open Browser Debugger
          </a>
        )}
        <button
          onClick={() => onResolve({ type: "Choice", data: "Done" }, "Done")}
          className="rounded-lg border border-border px-2.5 py-1 text-xs font-medium text-text-secondary hover:border-accent hover:text-accent transition"
        >
          Resume Agent
        </button>
      </div>
    </div>
  );
}

function defaultPrefix(query: string): string {
  return query.toUpperCase().replace(/[^A-Z0-9]+/g, "_").replace(/^_+|_+$/g, "");
}

export function CredentialContent({ te, onResolve }: ToolContentProps) {
  const hitl = te.hitl;
  const queryStr = hitl?.request.type === "Credential" ? hitl.request.data.query : "";
  const reasonStr = hitl?.request.type === "Credential" ? hitl.request.data.reason : "";

  const [connections, setConnections] = useState<PickerConnection[]>([]);
  const [selection, setSelection] = useState<CredentialOption | null>(null);
  const [loadingConnections, setLoadingConnections] = useState(true);
  const [connectionError, setConnectionError] = useState(false);
  const [connectionVersion, setConnectionVersion] = useState(0);
  const [duration, setDuration] = useState<GrantDuration>("once");
  const [bindingMode, setBindingMode] = useState<"prefix" | "single">("prefix");
  const [envVarPrefix, setEnvVarPrefix] = useState(defaultPrefix(queryStr));
  const [envVar, setEnvVar] = useState<string | null>(null);
  const [fields, setFields] = useState<string[]>([]);
  const [selectedField, setSelectedField] = useState("");
  const [loadingFields, setLoadingFields] = useState(false);
  const [fieldError, setFieldError] = useState(false);
  const [fieldVersion, setFieldVersion] = useState(0);
  const inputId = useId();
  const selectedConnectionId = selection?.connection_id;
  const selectedItemId = selection?.id;

  useEffect(() => {
    setFields([]);
    setSelectedField("");
    setFieldError(false);
    setLoadingFields(!!selectedItemId);
    if (!selectedConnectionId || !selectedItemId) return;
    const controller = new AbortController();
    api.get<string[]>(`/api/vaults/${encodeURIComponent(selectedConnectionId)}/items/${encodeURIComponent(selectedItemId)}/fields`)
      .then((availableFields) => {
        if (controller.signal.aborted) return;
        setFields(availableFields);
        setSelectedField(availableFields.includes("PASSWORD") ? "PASSWORD" : availableFields[0] ?? "");
      })
      .catch(() => { if (!controller.signal.aborted) setFieldError(true); })
      .finally(() => { if (!controller.signal.aborted) setLoadingFields(false); });
    return () => controller.abort();
  }, [selectedConnectionId, selectedItemId, fieldVersion]);

  useEffect(() => {
    const controller = new AbortController();
    setLoadingConnections(true);
    setConnectionError(false);
    api.get<PickerConnection[]>("/api/vaults")
      .then((conns) => { if (!controller.signal.aborted) setConnections(conns); })
      .catch(() => { if (!controller.signal.aborted) setConnectionError(true); })
      .finally(() => { if (!controller.signal.aborted) setLoadingConnections(false); });
    return () => controller.abort();
  }, [connectionVersion]);

  if (!hitl || hitl.request.type !== "Credential") return null;

  const prefix = envVarPrefix.trim();
  const variableName = envVar ?? (selectedField ? `${prefix ? `${prefix}_` : ""}${selectedField}` : "");
  const fieldsReady = !!selection && !loadingFields && !fieldError && fields.length > 0;

  const buildTarget = (): CredentialTarget | null => {
    if (bindingMode === "prefix") {
      if (!prefix) return null;
      return { Prefix: { env_var_prefix: prefix } };
    }
    const name = variableName.trim();
    if (!name || !selectedField) return null;
    const field: VaultField = selectedField === "PASSWORD" ? "Password"
      : selectedField === "USERNAME" ? "Username"
      : { Custom: { name: selectedField } };
    return { Single: { env_var: name, field } };
  };

  const target = buildTarget();

  const handleApprove = () => {
    if (!selection || !target || !fieldsReady) return;
    onResolve(
      {
        type: "Vault",
        data: {
          type: "Granted",
          data: {
            connection_id: selection.connection_id,
            vault_item_id: selection.id,
            grant_duration: duration,
            target,
          },
        },
      },
      "Approved",
    );
  };

  const handleDeny = () => {
    onResolve({ type: "Vault", data: { type: "Denied" } }, "Denied");
  };

  const durationValue = typeof duration === "string" ? duration : "hours" in duration ? "hours" : "days";

  return (
    <div className="space-y-3">
      <p className="text-sm text-text-tertiary">{reasonStr}</p>

      {loadingConnections ? (
        <p role="status" className="text-xs text-text-tertiary">Loading vaults...</p>
      ) : connectionError ? (
        <p role="alert" className="text-xs text-danger">
          Could not load vaults.{" "}
          <button type="button" onClick={() => setConnectionVersion((version) => version + 1)} className="underline">Retry</button>
        </p>
      ) : (
        <VaultItemPicker
          connections={connections}
          selection={selection}
          onSelect={(item) => {
            setSelection(item);
            setFields([]);
            setSelectedField("");
            setLoadingFields(!!item);
            setFieldVersion((version) => version + 1);
            if (item && !envVarPrefix) setEnvVarPrefix(defaultPrefix(queryStr) || defaultPrefix(item.name) || "CREDENTIAL");
          }}
          initialQuery={queryStr}
        />
      )}

      {selection && <fieldset className="space-y-2">
        <legend className="block text-sm font-medium text-text-tertiary mb-2">What should the agent use?</legend>
        <div className="flex gap-1.5">
          {([
            { value: "prefix", label: "Entire credential" },
            { value: "single", label: "A specific field" },
          ] as const).map((mode) => (
            <button
              key={mode.value}
              type="button"
              aria-pressed={bindingMode === mode.value}
              onClick={() => setBindingMode(mode.value)}
              className={`flex-1 rounded-lg border px-2.5 py-1.5 text-xs font-medium transition ${
                bindingMode === mode.value
                  ? "border-accent bg-accent/10 text-accent"
                  : "border-border text-text-secondary hover:border-accent"
              }`}
            >
              {mode.label}
            </button>
          ))}
        </div>
        {loadingFields ? (
          <p role="status" className="text-xs text-text-tertiary">Loading fields...</p>
        ) : fieldError ? (
          <p role="alert" className="text-xs text-danger">
            Could not load credential fields.{" "}
            <button type="button" className="underline" onClick={() => setFieldVersion((version) => version + 1)}>Retry</button>
          </p>
        ) : fields.length === 0 ? (
          <p className="text-xs text-text-tertiary">This credential has no available fields.</p>
        ) : bindingMode === "single" ? (
          <div className="space-y-2">
            <select
              id={`${inputId}-field`}
              aria-label="Field to share"
              value={selectedField}
              onChange={(e) => setSelectedField(e.target.value)}
              className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary"
            >
              {fields.map((field) => (
                <option key={field} value={field}>
                  {field === "PASSWORD" ? "Password" : field === "USERNAME" ? "Username" : field === "API_KEY" ? "API key" : field}
                </option>
              ))}
            </select>
          </div>
        ) : null}

        {fieldsReady && target && (
            <div className="flex flex-wrap gap-1.5" aria-label="Environment variable preview" aria-live="polite">
              {(bindingMode === "prefix" ? fields.map((field) => `${prefix}_${field}`) : [variableName.trim()]).map((name) => (
                <code key={name} className="rounded bg-surface-tertiary px-2 py-1 text-xs text-text-primary">{name}</code>
              ))}
            </div>
        )}

        <details className="text-xs text-text-tertiary">
          <summary className="cursor-pointer py-1">Advanced</summary>
          <div className="space-y-2 pt-2">
            {bindingMode === "prefix" ? (<>
              <label htmlFor={`${inputId}-prefix`} className="block font-medium">Variable name prefix</label>
              <input
                id={`${inputId}-prefix`}
                value={envVarPrefix}
                onChange={(e) => setEnvVarPrefix(e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, ""))}
                placeholder="DB"
                className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm font-mono text-text-primary placeholder:text-text-tertiary"
              />
            </>) : (<>
              <label htmlFor={`${inputId}-variable`} className="block font-medium">Environment variable name</label>
              <input
                id={`${inputId}-variable`}
                value={variableName}
                onChange={(e) => setEnvVar(e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, ""))}
                placeholder="DB_PASSWORD"
                className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm font-mono text-text-primary placeholder:text-text-tertiary"
              />
            </>)}
          </div>
        </details>
      </fieldset>}

      <div>
        <Label>Duration</Label>
        <select
          value={durationValue}
          onChange={(e) => {
            const v = e.target.value;
            if (v === "once") setDuration("once");
            else if (v === "permanent") setDuration("permanent");
            else if (v === "hours") setDuration({ hours: 24 });
            else if (v === "days") setDuration({ days: 7 });
          }}
          className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary"
        >
          <option value="once">Allow once</option>
          <option value="hours">Allow for 24 hours</option>
          <option value="days">Allow for 7 days</option>
          <option value="permanent">Allow permanently</option>
        </select>
      </div>

      <ApprovalButtons loading={false} onApprove={handleApprove} onDeny={handleDeny} approveDisabled={!fieldsReady || !target} />
    </div>
  );
}

export function AppContent({ te, onResolve }: ToolContentProps) {
  const hitl = te.hitl;
  if (!hitl || hitl.request.type !== "App") return null;
  const { action, manifest } = hitl.request.data;
  const name = String(manifest?.name || manifest?.id || "Unknown service");
  const description = manifest?.description ? String(manifest.description) : null;
  const command = manifest?.command ? String(manifest.command) : null;

  const handleApprove = () => {
    onResolve({ type: "Approval", data: true }, "Approved");
  };

  const handleDeny = () => {
    onResolve({ type: "Approval", data: false }, "Denied");
  };

  return (
    <div className="space-y-3">
      <div>
        <p className="text-sm font-medium text-text-primary capitalize">{action} service: {name}</p>
        {description && <p className="text-xs text-text-tertiary mt-1">{description}</p>}
      </div>
      {command && (
        <div>
          <Label>Command</Label>
          <code className="block rounded-lg border border-border bg-surface-secondary px-3 py-2 text-xs font-mono text-text-secondary overflow-x-auto">
            {command}
          </code>
        </div>
      )}
      <ApprovalButtons loading={false} onApprove={handleApprove} onDeny={handleDeny} />
    </div>
  );
}

export function ToolContentDispatch(props: ToolContentProps & { selectedAnswer?: string }) {
  switch (props.te.hitl?.request.type) {
    case "Question":
      return <QuestionContent {...props} />;
    case "Takeover":
      return <TakeoverContent {...props} />;
    case "Credential":
      return <CredentialContent {...props} />;
    case "App":
      return <AppContent {...props} />;
    default:
      return null;
  }
}
