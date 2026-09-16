"use client";

import type { ComponentType, SVGProps } from "react";
import type { MemoryConfig, ModelGroupConfig } from "@/lib/config-types";
import { formatGroupName } from "@/lib/model-groups";
import { HelpTip, NumberInput, SelectInput, SectionHeader, SectionPanel } from "@/components/settings/field";
import {
  ArrowPathIcon,
  ArrowsPointingInIcon,
  BookOpenIcon,
  BoltIcon,
  CheckIcon,
  CircleStackIcon,
  DocumentTextIcon,
  MagnifyingGlassIcon,
  ShareIcon,
} from "@heroicons/react/24/outline";

interface MemorySectionProps {
  memory: MemoryConfig;
  models: Record<string, ModelGroupConfig>;
  /** The backend the server is actually running (as loaded), which can differ
   *  from the edited selection until a restart. Gets the "Active" badge. */
  activeBackend?: MemoryConfig["backend"] | null;
  onChange: (memory: MemoryConfig) => void;
}

type BackendKey = NonNullable<MemoryConfig["backend"]>;

type BackendMeta = {
  title: string;
  tagline: string;
  description: string;
  features?: { name: string; detail: string }[];
  icon: ComponentType<SVGProps<SVGSVGElement>>;
  accentText: string;
  accentBg: string;
};

const backendOrder: BackendKey[] = ["basic", "pkm"];

const backendInfo: Record<BackendKey, BackendMeta> = {
  basic: {
    title: "Basic",
    tagline: "Rolling summaries",
    description:
      "Compacts loose memories into rolling summaries injected straight into the agent's context. Lightweight and low-overhead, with no page tree or background graph to maintain.",
    icon: DocumentTextIcon,
    accentText: "text-blue-400",
    accentBg: "bg-blue-400/15",
  },
  pkm: {
    title: "Ontology Memory",
    tagline: "A typed, reasoned knowledge graph — yours to read and edit.",
    description:
      "A living knowledge base your agent grows from your conversations, where every entity is typed and linked against a shared ontology and kept consistent by a background reasoner. Fully yours to read, edit, and reorganize.",
    features: [
      {
        name: "Typed entities & relations",
        detail:
          "each page is classified against a shared schema (schema.org, FOAF, plus your own frona: terms); relations are typed and their inverses inferred.",
      },
      {
        name: "Reasoning & consistency",
        detail:
          "a background reasoner materializes implied links and flags contradictions — questionable facts are quarantined, not trusted.",
      },
      {
        name: "Auto-consolidation",
        detail: "conversations distill into typed entities and linked pages in the background.",
      },
      {
        name: "Two-way sync",
        detail:
          "read and edit the agent's pages in Obsidian (or any Markdown editor); frontmatter is CURIE-keyed JSON-LD (liftable to RDF), and your own notes sync in for it to read.",
      },
      {
        name: "History & playbooks",
        detail: "facts supersede with full history; reusable playbooks capture how-to.",
      },
    ],
    icon: ShareIcon,
    accentText: "text-purple-400",
    accentBg: "bg-purple-400/15",
  },
};

export function MemorySection({ memory, models, activeBackend, onChange }: MemorySectionProps) {
  // `memory.backend` may be null (unconfigured) - resolve to the running backend so the
  // radio + backend-gated panels show a concrete selection. (The setup wizard preselects
  // PKM by setting memory.backend directly.)
  const effectiveBackend = memory.backend ?? activeBackend ?? "basic";
  // Defined model groups, plus the current value if it names a group that isn't
  // (yet) defined - so the select always shows the active selection.
  const groupOptions = Array.from(
    new Set([...Object.keys(models), memory.model_group].filter(Boolean)),
  ).map((g) => ({ value: g, label: formatGroupName(g) }));

  return (
    <div className="space-y-6">
      <SectionHeader
        title="Memory"
        description="How the agent remembers across conversations"
        icon={CircleStackIcon}
      />
      <SectionPanel>
        <div className="space-y-2">
          <div className="inline-flex items-center gap-1 text-sm font-medium text-text-secondary">
            Backend
            <HelpTip content="Which memory subsystem runs. Takes effect after a restart." />
          </div>

          <div
            role="radiogroup"
            aria-label="Memory backend"
            className="flex flex-col gap-3"
          >
            {backendOrder.map((key) => {
              const meta = backendInfo[key];
              const Icon = meta.icon;
              const selected = effectiveBackend === key;
              const active = key === activeBackend;
              return (
                <button
                  key={key}
                  type="button"
                  role="radio"
                  aria-checked={selected}
                  onClick={() => {
                    if (key !== effectiveBackend) onChange({ ...memory, backend: key });
                  }}
                  className={`text-left rounded-xl border p-4 transition focus:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
                    selected
                      ? "border-accent bg-accent/10 ring-1 ring-accent"
                      : "border-border bg-surface-secondary hover:bg-surface-tertiary"
                  }`}
                >
                  <div className="flex items-start gap-3">
                    <div className={`shrink-0 rounded-lg p-2 ${meta.accentBg} ${meta.accentText}`}>
                      <Icon className="h-5 w-5" />
                    </div>
                    <div className="min-w-0 flex-1 space-y-2">
                      <div className="flex items-start justify-between gap-2">
                        <div className="min-w-0">
                          <div className="flex items-center gap-2">
                            <span className="text-sm font-semibold text-text-primary">{meta.title}</span>
                            {active && (
                              <span className="shrink-0 rounded-full bg-green-500/15 px-2 py-0.5 text-[11px] font-medium text-green-500">
                                Active
                              </span>
                            )}
                          </div>
                          <div className="text-xs text-text-tertiary">{meta.tagline}</div>
                        </div>
                        <span
                          className={`mt-0.5 grid h-4 w-4 shrink-0 place-items-center rounded-full border ${
                            selected ? "border-accent" : "border-border"
                          }`}
                        >
                          {selected && <span className="h-2 w-2 rounded-full bg-accent" />}
                        </span>
                      </div>
                      <p className="text-sm leading-relaxed text-text-secondary">{meta.description}</p>
                      {meta.features && (
                        <ul className="space-y-1.5 pt-1">
                          {meta.features.map((f) => (
                            <li key={f.name} className="flex gap-2 text-sm text-text-secondary">
                              <CheckIcon className={`mt-0.5 h-4 w-4 shrink-0 ${meta.accentText}`} />
                              <span>
                                <span className="font-medium text-text-primary">{f.name}</span>
                                {": "}
                                {f.detail}
                              </span>
                            </li>
                          ))}
                        </ul>
                      )}
                    </div>
                  </div>
                </button>
              );
            })}
          </div>
        </div>

        <SelectInput
          label="Model group"
          description={
            effectiveBackend === "basic"
              ? "Model group for memory compaction. If undefined, uses the chat agent's model. Scheduled user and space compaction uses the most recently active chat."
              : "Model group for Ontology Memory consolidation. Falls back to primary if undefined."
          }
          value={memory.model_group}
          allowEmpty={false}
          onChange={(model_group) =>
            onChange({ ...memory, model_group: model_group ?? memory.model_group })
          }
          options={groupOptions}
        />
      </SectionPanel>

      {effectiveBackend === "basic" && (
        <SectionPanel title="Compaction" icon={ArrowsPointingInIcon}>
          <NumberInput
            label="Compaction token threshold"
            description="Skip user/agent memory compaction below this many tokens."
            value={memory.basic_compaction_token_threshold}
            onChange={(v) => onChange({ ...memory, basic_compaction_token_threshold: v })}
            min={0}
          />
          <NumberInput
            label="Compaction interval (hours)"
            description="How often to run user/agent memory compaction."
            value={Math.round(memory.basic_compaction_secs / 3600)}
            onChange={(hours) => onChange({ ...memory, basic_compaction_secs: hours * 3600 })}
            min={1}
          />
          <NumberInput
            label="Space compaction interval (hours)"
            description="How often to run space memory compaction."
            value={Math.round(memory.basic_space_compaction_secs / 3600)}
            onChange={(hours) => onChange({ ...memory, basic_space_compaction_secs: hours * 3600 })}
            min={1}
          />
        </SectionPanel>
      )}

      {effectiveBackend === "pkm" && (
        <>
          <SectionPanel title="Search" icon={MagnifyingGlassIcon}>
            <NumberInput
              label="Results (top-k)"
              description="Max hits returned by memory_search."
              value={memory.pkm_search_top_k}
              onChange={(v) => onChange({ ...memory, pkm_search_top_k: v })}
              min={1}
            />
          </SectionPanel>

          <SectionPanel title="Short-term memory" icon={BoltIcon}>
            <NumberInput
              label="Half-life (seconds)"
              description="Recency-decay half-life for short memory."
              value={memory.pkm_short_memory_half_life_secs}
              onChange={(v) => onChange({ ...memory, pkm_short_memory_half_life_secs: v })}
              min={1}
            />
            <NumberInput
              label="Demote threshold"
              description="Drop short memory once its decay score falls below this."
              value={memory.pkm_short_memory_demote_threshold}
              onChange={(v) => onChange({ ...memory, pkm_short_memory_demote_threshold: v })}
              min={0}
              step={0.05}
            />
            <NumberInput
              label="Max lines (top-n)"
              description="Max short-memory lines injected into the <short_memory> block."
              value={memory.pkm_short_memory_top_n}
              onChange={(v) => onChange({ ...memory, pkm_short_memory_top_n: v })}
              min={0}
            />
            <NumberInput
              label="Token cap"
              description="Token budget for the <short_memory> block."
              value={memory.pkm_short_memory_token_cap}
              onChange={(v) => onChange({ ...memory, pkm_short_memory_token_cap: v })}
              min={0}
            />
          </SectionPanel>

          <SectionPanel title="Consolidation" icon={ArrowPathIcon}>
            <NumberInput
              label="Sweep interval (seconds)"
              description="How often the consolidation sweep scans for idle chats."
              value={memory.pkm_consolidate_secs}
              onChange={(v) => onChange({ ...memory, pkm_consolidate_secs: v })}
              min={1}
            />
            <NumberInput
              label="Idle before consolidate (seconds)"
              description="How long a chat must be quiet before it's consolidated."
              value={memory.pkm_consolidate_idle_secs}
              onChange={(v) => onChange({ ...memory, pkm_consolidate_idle_secs: v })}
              min={1}
            />
            <NumberInput
              label="Concurrent model calls"
              description="Maximum consolidation model calls that may run at once across chat mining and page authoring."
              value={memory.pkm_consolidation_concurrency}
              onChange={(v) => onChange({ ...memory, pkm_consolidation_concurrency: v })}
              min={1}
            />
            <NumberInput
              label="Consolidation tool turns"
              description="Maximum exploration-tool turns for Classify and Resolve. Zero disables exploration tools."
              value={memory.pkm_consolidation_max_tool_turns}
              onChange={(v) => onChange({ ...memory, pkm_consolidation_max_tool_turns: v })}
              min={0}
            />
            <NumberInput
              label="Consolidation submissions"
              description="Maximum structured submission attempts for Classify, Resolve, and Reconcile."
              value={memory.pkm_consolidation_max_submissions}
              onChange={(v) => onChange({ ...memory, pkm_consolidation_max_submissions: v })}
              min={1}
            />
            <NumberInput
              label="Stage retry limit"
              description="Maximum failures at the current consolidation stage before abandoning the pass."
              value={memory.pkm_consolidation_max_attempts}
              onChange={(v) => onChange({ ...memory, pkm_consolidation_max_attempts: v })}
              min={1}
            />
            <NumberInput
              label="Adjudication attempts per batch"
              description="Maximum adjudication submissions per batch, including revisions requested by guardrails."
              value={memory.pkm_adjudication_max_attempts_per_batch}
              onChange={(v) => onChange({ ...memory, pkm_adjudication_max_attempts_per_batch: v })}
              min={1}
            />
            <NumberInput
              label="Checkpoint recovery limit"
              description="Fatal post-extraction checkpoint resets allowed before the pass fails permanently."
              value={memory.pkm_consolidation_checkpoint_failure_cap}
              onChange={(v) => onChange({ ...memory, pkm_consolidation_checkpoint_failure_cap: v })}
              min={0}
            />
            <NumberInput
              label="Retry backoff (seconds)"
              description="Base delay for failed consolidation retries. The delay doubles after each attempt."
              value={memory.pkm_consolidation_retry_base_secs}
              onChange={(v) => onChange({ ...memory, pkm_consolidation_retry_base_secs: v })}
              min={0}
            />
          </SectionPanel>

          <SectionPanel title="Extraction" icon={DocumentTextIcon}>
            <NumberInput
              label="Transcript token cap"
              description="Maximum estimated transcript tokens sent to Extract in one request."
              value={memory.pkm_extract_max_tokens}
              onChange={(v) => onChange({ ...memory, pkm_extract_max_tokens: v })}
              min={1}
            />
            <NumberInput
              label="Transcript message cap"
              description="Maximum messages consumed by one Extract request."
              value={memory.pkm_extract_max_messages}
              onChange={(v) => onChange({ ...memory, pkm_extract_max_messages: v })}
              min={1}
            />
            <NumberInput
              label="Tool-evidence lookback"
              description="Same-chat agent messages searched backward for successful tool evidence."
              value={memory.pkm_extract_agent_evidence_lookback_messages}
              onChange={(v) => onChange({ ...memory, pkm_extract_agent_evidence_lookback_messages: v })}
              min={0}
            />
            <NumberInput
              label="Tool-evidence result token cap"
              description="Token cap returned by each scoped tool-evidence search or read."
              value={memory.pkm_extract_agent_evidence_result_token_cap}
              onChange={(v) => onChange({ ...memory, pkm_extract_agent_evidence_result_token_cap: v })}
              min={0}
            />
          </SectionPanel>

          <SectionPanel title="Playbooks" icon={BookOpenIcon}>
            <NumberInput
              label="Index token cap"
              description="Token budget for the available-playbooks index. Least-used playbooks are dropped first when it is exceeded."
              value={memory.pkm_playbook_index_token_cap}
              onChange={(v) => onChange({ ...memory, pkm_playbook_index_token_cap: v })}
              min={0}
            />
            <NumberInput
              label="Playbook tool turns"
              description="Maximum exploration-tool turns for Playbook Resolve and Playbook Author. Zero disables exploration tools."
              value={memory.pkm_playbook_max_tool_turns}
              onChange={(v) => onChange({ ...memory, pkm_playbook_max_tool_turns: v })}
              min={0}
            />
            <NumberInput
              label="Playbook submissions"
              description="Maximum structured submission attempts for Playbook Resolve and Playbook Author."
              value={memory.pkm_playbook_max_submissions}
              onChange={(v) => onChange({ ...memory, pkm_playbook_max_submissions: v })}
              min={1}
            />
          </SectionPanel>

        </>
      )}
    </div>
  );
}
