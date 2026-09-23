"use client";

import { useState, useEffect, useCallback, useMemo } from "react";
import { useRouter } from "next/navigation";
import { RequireAuth } from "@/components/require-auth";
import { api } from "@/lib/api-client";
import { CheckCircleIcon, ClockIcon, InformationCircleIcon } from "@heroicons/react/24/outline";
import { SectionHeader, SectionPanel } from "@/components/settings/field";
import { ComboboxInput } from "@/components/settings/combobox";
import type { ServerConfig } from "@/lib/config-types";
import { ProvidersSection } from "@/components/settings/sections/providers-section";
import { ModelsSection } from "@/components/settings/sections/models-section";
import { MemorySection } from "@/components/settings/sections/memory-section";
import { ServerSection } from "@/components/settings/sections/server-section";
import { AuthSection } from "@/components/settings/sections/auth-section";
import { SsoSection } from "@/components/settings/sections/sso-section";
import { BrowserSection } from "@/components/settings/sections/browser-section";
import { SearchSection } from "@/components/settings/sections/search-section";
import { VoiceSection } from "@/components/settings/sections/voice-section";
import { SandboxSettingsSection } from "@/components/settings/sections/sandbox-section";
import { getConfigDocument, updateConfig, isSensitiveSet } from "@/lib/config-types";
import type { Config, ConfigUpdateResponse } from "@/lib/config-types";
import { modelGroupsPatch } from "@/lib/model-authoring";
import { acceptProviderDrafts, type ProviderDrafts } from "@/lib/provider-drafts";
import { Logo } from "@/components/logo";

function generateStrongSecret(length: number): string {
  const charset = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
  const bytes = crypto.getRandomValues(new Uint8Array(length));
  let result = "";
  for (let i = 0; i < length; i++) {
    result += charset[bytes[i] % charset.length];
  }
  return result;
}

const STEPS = [
  { id: "timezone" },
  { id: "server" },
  { id: "providers" },
  { id: "models" },
  { id: "memory" },
  { id: "auth" },
  { id: "sso" },
  { id: "sandbox" },
  { id: "browser" },
  { id: "search" },
  { id: "voice" },
] as const;

const TIMEZONE_AUTO_DETECT = "(auto-detect from server environment)";

function TimezoneStep({ server, onChange }: { server: ServerConfig; onChange: (server: ServerConfig) => void }) {
  const [timezones, setTimezones] = useState<string[]>([]);

  useEffect(() => {
    api.get<string[]>("/api/system/timezones").then(setTimezones).catch(() => {});
  }, []);

  const timezoneItems = useMemo(
    () => [
      { value: "", label: TIMEZONE_AUTO_DETECT },
      ...timezones.map((tz) => ({ value: tz, label: tz.replace(/_/g, " ") })),
    ],
    [timezones],
  );

  const browserTimezone = useMemo(() => {
    try {
      return Intl.DateTimeFormat().resolvedOptions().timeZone;
    } catch {
      return null;
    }
  }, []);

  return (
    <div>
      <SectionHeader
        title="Timezone"
        description="The default timezone used for scheduling, reminders, and the time context shown to agents."
        icon={ClockIcon}
      />
      <div className="flex items-start gap-3 rounded-lg border border-accent/30 bg-accent/5 p-4 mb-4">
        <InformationCircleIcon className="h-5 w-5 text-accent shrink-0 mt-0.5" />
        <p className="text-sm text-text-secondary leading-relaxed">
          This is the server default. Each user can override it in their own settings, and individual tasks can specify a different zone. Daylight saving transitions are handled automatically.
        </p>
      </div>
      <SectionPanel>
        <ComboboxInput
          label="Default Timezone"
          description="Leave on auto-detect to read the TZ env var or /etc/localtime at startup, falling back to UTC."
          value={server.timezone}
          items={timezoneItems}
          onChange={(timezone) => onChange({ ...server, timezone })}
          placeholder={TIMEZONE_AUTO_DETECT}
          allowFreeText={false}
        />
        {browserTimezone && server.timezone !== browserTimezone && (
          <button
            type="button"
            onClick={() => onChange({ ...server, timezone: browserTimezone })}
            className="text-xs text-accent hover:underline"
          >
            Use browser timezone: {browserTimezone}
          </button>
        )}
      </SectionPanel>
    </div>
  );
}

export default function SetupPage() {
  return (
    <RequireAuth>
      <SetupWizard />
    </RequireAuth>
  );
}

function SetupComplete() {
  const router = useRouter();
  const [restarting, setRestarting] = useState(false);

  const handleRestart = async () => {
    setRestarting(true);
    try {
      await api.post("/api/system/restart", {});
    } catch {
      // Server may drop connection during restart
    }
    setTimeout(() => {
      router.push("/home");
    }, 3000);
  };

  return (
    <div className="flex min-h-screen items-center justify-center px-4 bg-surface">
      <div className="w-full max-w-md space-y-6 text-center">
        <CheckCircleIcon className="h-16 w-16 text-accent mx-auto" />
        <h1 className="text-2xl font-bold text-text-primary">Setup Complete</h1>
        <p className="text-text-secondary">
          Your settings have been saved to <code className="text-sm bg-surface-secondary px-1.5 py-0.5 rounded">config.yaml</code>.
          The server needs to restart for changes to take effect.
        </p>

        {restarting ? (
          <div className="space-y-3 pt-2">
            <div className="flex items-center justify-center gap-2 text-sm text-text-tertiary">
              <svg className="h-4 w-4 animate-spin" viewBox="0 0 24 24" fill="none">
                <circle className="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" strokeWidth="4" />
                <path className="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z" />
              </svg>
              Restarting server...
            </div>
          </div>
        ) : (
          <div className="flex flex-col items-center gap-3 pt-2">
            <button
              onClick={handleRestart}
              className="rounded-lg bg-accent px-6 py-2.5 text-sm font-medium text-surface hover:bg-accent-hover transition"
            >
              Restart Server
            </button>
            <button
              onClick={() => router.push("/settings")}
              className="text-sm text-text-tertiary hover:text-text-secondary transition"
            >
              Skip, I&apos;ll restart later
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

function SetupWizard() {
  const router = useRouter();
  const [config, setConfig] = useState<Config | null>(null);
  const [savedConfig, setSavedConfig] = useState<Config | null>(null);
  const [patch, setPatch] = useState<Record<string, unknown>>({});
  const [persistedRevision, setPersistedRevision] = useState("");
  const [providerDrafts, setProviderDrafts] = useState<ProviderDrafts>({});
  const [step, setStep] = useState(0);
  const [saving, setSaving] = useState(false);
  const [completed, setCompleted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [providersBlock, setProvidersBlock] = useState<string | null>(null);
  const [modelsBlock, setModelsBlock] = useState<string | null>(null);

  const updatePatch = useCallback((section: string, value: unknown) => {
    setPatch((prev) => ({ ...prev, [section]: value }));
    setConfig((prev) => prev ? { ...prev, [section]: value } as Config : prev);
  }, []);

  const updateModels = useCallback((models: Config["models"]) => {
    setPatch(previous => {
      const next = { ...previous };
      const changes = modelGroupsPatch(savedConfig?.models ?? {}, models);
      if (Object.keys(changes).length) next.models = changes;
      else delete next.models;
      return next;
    });
    setConfig(previous => previous ? { ...previous, models } : previous);
  }, [savedConfig]);

  const updateProviders = useCallback((providers: Config["providers"], removed: string[] = []) => {
    setPatch(previous => {
      const value: Record<string, unknown> = { ...(previous.providers as Record<string, unknown> ?? {}), ...providers };
      for (const handle of removed) value[handle] = null;
      return { ...previous, providers: value };
    });
    setConfig(previous => previous ? { ...previous, providers } : previous);
  }, []);
  const providerSaved = useCallback((result: ConfigUpdateResponse) => {
    setConfig(result.config); setSavedConfig(result.config); setPersistedRevision(result.persisted_revision); setPatch({}); setProviderDrafts({});
  }, []);

  useEffect(() => {
    getConfigDocument()
      .then((document) => {
        const cfg = document.config;
        setPersistedRevision(document.persisted_revision);
        setConfig(cfg); setSavedConfig(cfg);
        if (!isSensitiveSet(cfg.auth.encryption_secret)) {
          const secret = generateStrongSecret(64);
          updatePatch("auth", { ...cfg.auth, encryption_secret: secret });
        }
        // Preselect PKM for a *new* install (backend unconfigured) so it saves an
        // explicit `pkm`. Only when unset - never override an explicit `basic`/`pkm`
        // the operator already chose. The user can still switch it in the memory step.
        if (cfg.memory.backend == null) {
          updatePatch("memory", { ...cfg.memory, backend: "pkm" });
        }
      })
      .catch(() => setError("Failed to load configuration"))
      .finally(() => setLoading(false));
  }, [updatePatch]);

  const currentStep = STEPS[step];
  const isLastStep = step === STEPS.length - 1;

  function getBlockReason(): string | null {
    if (currentStep.id === "providers") return providersBlock;
    if (currentStep.id === "models") return modelsBlock;
    if (currentStep.id === "auth" && config) {
      const secret = config.auth.encryption_secret;
      const hasSecret = typeof secret === "string" ? secret.length > 0 : (typeof secret === "object" && secret?.is_set);
      if (!hasSecret) return "Encryption secret is required";
    }
    return null;
  }
  const blockReason = getBlockReason();
  const canAdvance = !blockReason;

  const handleComplete = useCallback(async () => {
    setSaving(true);
    setError(null);
    try {
      if (providersBlock) throw new Error(providersBlock);
      if (modelsBlock) throw new Error(modelsBlock);
      const acceptedPatch = await acceptProviderDrafts(patch, providerDrafts, (handle, connection) => {
        setConfig(previous => previous ? { ...previous, providers: { ...previous.providers, [handle]: connection } } : previous);
        setPatch(previous => ({ ...previous, providers: { ...(previous.providers as Record<string, unknown>), [handle]: connection } }));
        setProviderDrafts(previous => { const next = { ...previous }; delete next[handle]; return next; });
      });
      const result = await updateConfig(acceptedPatch, { expectedPersistedRevision: persistedRevision, baseline: savedConfig });
      setConfig(result.config); setSavedConfig(result.config);
      setPersistedRevision(result.persisted_revision);
      setProviderDrafts({});
      setPatch({});
      setCompleted(true);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save configuration");
    } finally {
      setSaving(false);
    }
  }, [patch, persistedRevision, providersBlock, modelsBlock, providerDrafts, savedConfig]);

  if (loading) {
    return (
      <div className="flex min-h-screen items-center justify-center">
        <p className="text-sm text-text-tertiary">Loading configuration...</p>
      </div>
    );
  }

  if (completed) {
    return <SetupComplete />;
  }

  if (!config) {
    return (
      <div className="flex min-h-screen items-center justify-center">
        <p className="text-sm text-error-text">{error || "Failed to load configuration"}</p>
      </div>
    );
  }

  return (
    <div className="flex min-h-screen flex-col bg-surface">
      {/* Header */}
      <div className="border-b border-border bg-surface-secondary px-6 py-4">
        <div className="max-w-2xl mx-auto">
          <div className="relative flex items-center justify-center mb-6">
            <div className="absolute left-0 flex items-center gap-2">
              <Logo size={56} />
              <span className="text-2xl font-bold text-text-primary tracking-wide" style={{ fontFamily: "var(--font-brand)" }}>FRONA</span>
            </div>
            <span className="text-sm text-text-tertiary">{step + 1} of {STEPS.length}</span>
            <h1 className="absolute right-0 text-2xl font-bold text-text-primary tracking-wide" style={{ fontFamily: "var(--font-brand)" }}>
              SETUP
            </h1>
          </div>
          <div className="w-full bg-surface-tertiary rounded-full h-1.5">
            <div
              className="bg-accent h-1.5 rounded-full transition-all"
              style={{ width: `${((step + 1) / STEPS.length) * 100}%` }}
            />
          </div>
        </div>
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto">
        <div className="max-w-2xl mx-auto px-6 py-8 space-y-6">
          {error && (
            <div className="rounded-lg bg-error-bg p-3 text-sm text-error-text">{error}</div>
          )}

          <div className="min-h-[300px]">
            {currentStep.id === "timezone" && (
              <TimezoneStep
                server={config.server}
                onChange={(v) => updatePatch("server", v)}
              />
            )}
            {currentStep.id === "providers" && (
              <ProvidersSection
                providers={config.providers}
                onChange={updateProviders}
                drafts={providerDrafts} onDraftsChange={setProviderDrafts}
                persistedRevision={persistedRevision} hasUnsavedChanges={Object.keys(patch).length > 0}
                onSaved={providerSaved} requireEnabledProvider
                onReadyChange={setProvidersBlock}
              />
            )}
            {currentStep.id === "models" && (
              <ModelsSection
                models={config.models}
                savedModels={savedConfig?.models}
                enabledProviders={Object.entries(config.providers)
                  .filter(([, provider]) => provider.enabled !== false)
                  .map(([id]) => id)}
                providerConfigs={config.providers}
                savedProviderConfigs={savedConfig?.providers} providerDrafts={providerDrafts}
                onChange={updateModels}
                onReadyChange={setModelsBlock}
              />
            )}
            {currentStep.id === "memory" && (
              <MemorySection
                memory={config.memory}
                models={config.models}
                activeBackend={null}
                onChange={(v) => updatePatch("memory", v)}
              />
            )}
            {currentStep.id === "server" && (
              <ServerSection
                server={config.server}
                onChange={(v) => updatePatch("server", v)}
              />
            )}
            {currentStep.id === "auth" && (
              <AuthSection
                auth={config.auth}
                onChange={(v) => updatePatch("auth", v)}
              />
            )}
            {currentStep.id === "sso" && (
              <SsoSection
                sso={config.sso}
                onChange={(v) => updatePatch("sso", v)}
                hasBaseUrl={!!(config.server.base_url || config.server.backend_url || config.server.external_url)}
              />
            )}
            {currentStep.id === "sandbox" && (
              <SandboxSettingsSection
                sandbox={config.sandbox}
                onChange={(v) => updatePatch("sandbox", v)}
              />
            )}
            {currentStep.id === "browser" && (
              <BrowserSection
                browser={config.browser}
                onChange={(v) => updatePatch("browser", v)}
              />
            )}
            {currentStep.id === "search" && (
              <SearchSection
                search={config.search}
                onChange={(v) => updatePatch("search", v)}
              />
            )}
            {currentStep.id === "voice" && (
              <VoiceSection
                voice={config.voice}
                onChange={(v) => updatePatch("voice", v)}
              />
            )}
          </div>
        </div>
      </div>

      {/* Navigation */}
      <div className="border-t border-border bg-surface-secondary px-6 py-4">
        <div className="max-w-2xl mx-auto flex items-center justify-between">
          <button
            onClick={() => setStep((s) => Math.max(0, s - 1))}
            disabled={step === 0}
            className="rounded-lg px-4 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary disabled:opacity-30 transition"
          >
            Back
          </button>
          {blockReason && (
            <p className="text-xs text-text-tertiary">
              {blockReason}
            </p>
          )}
          <div className="flex items-center gap-3">
            {!isLastStep && (
              <button
                onClick={() => setStep((s) => s + 1)}
                disabled={!canAdvance}
                className="rounded-lg px-4 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary disabled:opacity-30 disabled:cursor-not-allowed transition"
              >
                Skip
              </button>
            )}
            {isLastStep ? (
              <button
                onClick={handleComplete}
                disabled={saving}
                className="rounded-lg bg-accent px-6 py-2 text-sm font-medium text-surface hover:bg-accent-hover disabled:opacity-50 transition"
              >
                {saving ? "Saving..." : "Complete Setup"}
              </button>
            ) : (
              <button
                onClick={() => setStep((s) => s + 1)}
                disabled={!canAdvance}
                className="rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface hover:bg-accent-hover disabled:opacity-50 disabled:cursor-not-allowed transition"
              >
                Next
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
