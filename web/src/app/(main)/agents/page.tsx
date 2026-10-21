"use client";

import { ConfigurationSidebar } from "@/components/layout/configuration-sidebar";

import { useState, useEffect, useCallback, useRef, Suspense } from "react";
import { useSearchParams, useRouter } from "next/navigation";
import { api } from "@/lib/api-client";
import { useNavigation } from "@/lib/navigation-context";
import { getConfig, type SandboxConfig } from "@/lib/config-types";
import type { Agent, SandboxLimits, SandboxPolicy } from "@/lib/types";

/** Legacy "merged" shape the SandboxSection still edits. We project it from
 *  the agent's `sandbox_policy` (Cedar-evaluated) + `sandbox_limits` on load,
 *  and split it back into `{ sandbox_policy, sandbox_limits }` on save.
 *
 *  `shared_paths` carries an explicit per-entry write flag so users can grant
 *  read-only or read-write access independently. */
interface SharedPath {
  path: string;
  write: boolean;
}

interface SandboxFormShape {
  network_access: boolean;
  allowed_network_destinations: string[];
  timeout_secs?: number;
  max_cpu_pct?: number;
  max_memory_pct?: number;
  shared_paths: SharedPath[];
}

function fromAgent(agent: Agent): SandboxFormShape {
  const p = agent.sandbox_policy ?? {};
  const l = agent.sandbox_limits;
  const reads = p.read_paths ?? [];
  const writes = new Set(p.write_paths ?? []);
  const seen = new Set<string>();
  const shared_paths: SharedPath[] = [];
  for (const path of [...reads, ...(p.write_paths ?? [])]) {
    if (seen.has(path)) continue;
    seen.add(path);
    shared_paths.push({ path, write: writes.has(path) });
  }
  return {
    network_access: p.network_access ?? true,
    allowed_network_destinations: p.network_destinations ?? [],
    shared_paths,
    max_cpu_pct: l?.max_cpu_pct,
    max_memory_pct: l?.max_memory_pct,
    timeout_secs: l?.timeout_secs,
  };
}

function toRequest(
  s: SandboxFormShape,
  defaults: SandboxConfig | null,
): { sandbox_policy: SandboxPolicy; sandbox_limits?: SandboxLimits } {
  const entries = s.shared_paths.filter((e) => e.path);
  const sandbox_policy: SandboxPolicy = {
    network_access: s.network_access,
    network_destinations: s.allowed_network_destinations.filter(Boolean),
    read_paths: entries.map((e) => e.path),
    write_paths: entries.filter((e) => e.write).map((e) => e.path),
  };
  // The form treats an empty field as "use the server default" (the input
  // placeholders show those defaults). SandboxLimits on the wire requires all
  // three fields, so fill the unset ones from the server config whenever the
  // user has customised at least one.
  const anySet =
    s.max_cpu_pct !== undefined || s.max_memory_pct !== undefined || s.timeout_secs !== undefined;
  const limits =
    anySet && defaults
      ? {
          max_cpu_pct: s.max_cpu_pct ?? defaults.max_cpu_pct,
          max_memory_pct: s.max_memory_pct ?? defaults.max_memory_pct,
          timeout_secs: s.timeout_secs ?? defaults.timeout_secs,
        }
      : undefined;
  return { sandbox_policy, sandbox_limits: limits };
}
import { agentDisplayName } from "@/lib/types";
import { ProfileSection } from "@/components/agents/configure/profile-section";
import { InstructionsSection } from "@/components/agents/configure/instructions-section";
import { ModelSection } from "@/components/agents/configure/model-section";
import { ToolsSection } from "@/components/agents/configure/tools-section";
import { SkillsSection } from "@/components/agents/configure/skills-section";
import type { SkillBrowserHandle } from "@/components/skills/skill-browser";
import { SandboxSection } from "@/components/agents/configure/sandbox-section";
import { CredsSection } from "@/components/agents/configure/creds-section";

const SECTIONS = [
  { id: "profile", label: "Profile" },
  { id: "model", label: "Model" },
  { id: "prompt", label: "Prompt" },
  { id: "tools", label: "Tools" },
  { id: "skills", label: "Skills" },
  { id: "sandbox", label: "Sandbox" },
  { id: "creds", label: "Credentials" },
] as const;

type SectionId = (typeof SECTIONS)[number]["id"];

function AgentSettings() {
  const searchParams = useSearchParams();
  const router = useRouter();
  const { updateAgent } = useNavigation();
  const agentId = searchParams.get("id");

  const [agent, setAgent] = useState<Agent | null>(null);
  const [patch, setPatch] = useState<Record<string, unknown>>({});
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const sectionParam = searchParams.get("section");
  const initialSection = SECTIONS.some((s) => s.id === sectionParam) ? (sectionParam as SectionId) : "profile";
  const [activeSection, setActiveSectionState] = useState<SectionId>(initialSection);

  const setActiveSection = useCallback((id: SectionId) => {
    setActiveSectionState(id);
    const params = new URLSearchParams(searchParams.toString());
    params.set("section", id);
    router.replace(`/agents?${params.toString()}`);
  }, [searchParams, router]);
  const [hasAgentRemovals, setHasAgentRemovals] = useState(false);
  const [sandboxValid, setSandboxValid] = useState(true);
  const [sandboxDefaults, setSandboxDefaults] = useState<SandboxConfig | null>(null);
  const skillBrowserRef = useRef<SkillBrowserHandle>(null);

  useEffect(() => {
    if (!agentId) return;
    setLoading(true);
    api
      .get<Agent>(`/api/agents/${agentId}`)
      .then(setAgent)
      .catch(() => setError("Agent not found"))
      .finally(() => setLoading(false));
  }, [agentId]);

  useEffect(() => {
    getConfig().then((c) => setSandboxDefaults(c.sandbox)).catch(() => {});
  }, []);

  const merged = agent ? { ...agent, ...patch } : null;
  const hasPendingChanges = Object.keys(patch).length > 0 || hasAgentRemovals;

  const doSave = useCallback(async () => {
    if (!hasPendingChanges || !agent || !agentId) return;
    setSaving(true);
    setError(null);
    try {
      const payload: Record<string, unknown> = { ...patch };
      if (typeof payload.prompt === "string" && payload.prompt === agent.default_prompt) {
        payload.prompt = null;
      }
      if (payload.skills === null) {
        payload.skills = ["*"];
      }
      if (payload.sandbox_form !== undefined) {
        const form = payload.sandbox_form as SandboxFormShape;
        const split = toRequest(form, sandboxDefaults);
        payload.sandbox_policy = split.sandbox_policy;
        if (split.sandbox_limits) payload.sandbox_limits = split.sandbox_limits;
        delete payload.sandbox_form;
      }
      const updated = await api.put<Agent>(`/api/agents/${agentId}`, payload);
      setAgent(updated);
      setPatch({});
      updateAgent(agentId, { ...updated });
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save");
    } finally {
      setSaving(false);
    }
  }, [patch, hasPendingChanges, agent, agentId, updateAgent, sandboxDefaults]);

  const handleSave = useCallback(() => {
    if (skillBrowserRef.current) {
      skillBrowserRef.current.confirmRemovals(doSave);
    } else {
      doSave();
    }
  }, [doSave]);

  const handleDiscard = useCallback(() => {
    setPatch({});
    skillBrowserRef.current?.resetRemovals();
  }, []);

  const update = useCallback((fields: Record<string, unknown>) => {
    setPatch((prev) => ({ ...prev, ...fields }));
  }, []);

  if (!agentId) {
    return (
      <div className="flex h-full items-center justify-center">
        <p className="text-sm text-text-tertiary">No agent selected</p>
      </div>
    );
  }

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <p className="text-sm text-text-tertiary">Loading...</p>
      </div>
    );
  }

  if (!agent || !merged) {
    return (
      <div className="flex h-full items-center justify-center">
        <p className="text-sm text-error-text">{error || "Agent not found"}</p>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 min-w-0 bg-surface">
      {/* Sidebar */}
      <ConfigurationSidebar
        backHref="/chat"
        backLabel="Back"
        sections={SECTIONS}
        activeSection={activeSection}
        onSectionChange={setActiveSection}
        header={
          <h2 className="text-lg font-semibold text-text-primary mb-4 truncate">
            {agentDisplayName(agent.id, agent.name)}
          </h2>
        }
      />

      {/* Content */}
      <div className={`flex-1 min-h-0 min-w-0 overflow-y-auto [overflow-wrap:anywhere] ${activeSection === "prompt" ? "flex flex-col" : ""}`}>
        <div className={`w-full max-w-2xl mx-auto p-4 md:p-8 space-y-6 ${activeSection === "prompt" ? "flex-1 min-h-0 flex flex-col" : ""}`}>
          {error && (
            <div className="rounded-lg bg-error-bg p-3 text-sm text-error-text">{error}</div>
          )}

          <div className={activeSection === "prompt" ? "flex-1 min-h-0 flex flex-col" : ""}>
            {activeSection === "profile" && (
              <ProfileSection
                agentId={agentId!}
                description={(merged.description as string) ?? ""}
                enabled={(merged.enabled as boolean) ?? true}
                identity={(merged.identity as Record<string, string>) ?? {}}
                avatarUrl={(merged.avatar_url as string | null) ?? null}
                onChange={update}
                onIdentityChange={(v) => update({ identity: v })}
              />
            )}
            {activeSection === "model" && (
              <ModelSection
                modelGroup={(merged.model_group as string) ?? "primary"}
                onModelGroupChange={(v) => update({ model_group: v })}
              />
            )}
            {activeSection === "prompt" && (
              <InstructionsSection
                prompt={((merged.prompt as string) ?? agent.default_prompt)}
                onPromptChange={(v) => update({ prompt: v })}
              />
            )}
            {activeSection === "tools" && (
              <ToolsSection
                tools={(merged.tools as string[]) ?? []}
                onChange={(v) => update({ tools: v })}
              />
            )}
            {activeSection === "skills" && (
              <SkillsSection
                ref={skillBrowserRef}
                agentId={agentId}
                skills={"skills" in patch ? (patch.skills as string[] | null) : (agent.skills ?? null)}
                onSkillsChange={(v) => update({ skills: v })}
                onAgentRemovalsChange={setHasAgentRemovals}
              />
            )}
            {activeSection === "sandbox" && (
              <SandboxSection
                sandbox={(patch.sandbox_form as SandboxFormShape | undefined) ?? fromAgent(merged)}
                onChange={(v) => update({ sandbox_form: v })}
                onValidChange={setSandboxValid}
              />
            )}
            {activeSection === "creds" && <CredsSection principalKind="agent" principalId={agentId} />}
          </div>

          {/* Save bar */}
          {activeSection !== "creds" && (
            <div className="pt-4 pb-2 border-t border-border flex items-center justify-end gap-2">
              <button
                onClick={handleDiscard}
                disabled={!hasPendingChanges}
                className="w-28 rounded-lg border border-border py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary disabled:opacity-50 transition"
              >
                Discard
              </button>
              <button
                onClick={handleSave}
                disabled={!hasPendingChanges || saving || !sandboxValid}
                className="w-28 rounded-lg bg-accent py-2 text-sm font-medium text-surface hover:bg-accent-hover disabled:opacity-50 transition"
              >
                {saving ? "Saving..." : "Save"}
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

export default function AgentSettingsPage() {
  return (
    <Suspense fallback={<div className="flex h-full items-center justify-center"><p className="text-sm text-text-tertiary">Loading...</p></div>}>
      <AgentSettings />
    </Suspense>
  );
}
