"use client";
import { useState } from "react";
import type { ModelProviderConfig } from "@/lib/config-types";
import type { ModelDirectory } from "@/lib/provider-admin";
import { ComboboxInput } from "@/components/settings/combobox";
import { formatGroupName } from "@/lib/model-groups";

interface ModelSelectorProps {
  provider: string; model: string; enabledProviders: string[];
  providerConfigs: Record<string, ModelProviderConfig>; directory?: ModelDirectory;
  loading?: boolean;
  onProviderChange: (provider: string) => void; onModelChange: (model: string) => void;
}

const PROVIDER_LABELS: Record<string, string> = {
  anthropic: "Anthropic",
  openai: "OpenAI",
  groq: "Groq",
  openrouter: "OpenRouter",
  deepseek: "DeepSeek",
  gemini: "Gemini",
  cohere: "Cohere",
  mistral: "Mistral",
  perplexity: "Perplexity",
  together: "Together",
  xai: "xAI",
  hyperbolic: "Hyperbolic",
  moonshot: "Moonshot",
  mira: "Mira",
  galadriel: "Galadriel",
  huggingface: "Hugging Face",
  ollama: "Ollama",
};

export function ModelSelector({ provider, model, enabledProviders, providerConfigs, directory, loading,
  onProviderChange, onModelChange }: ModelSelectorProps) {
  const [draft, setDraft] = useState(model);
  const [previous, setPrevious] = useState(model);
  if (previous !== model) { setPrevious(model); setDraft(model); }
  function providerLabel(handle: string) {
    const brand = providerConfigs[handle]?.provider ?? handle;
    const label = PROVIDER_LABELS[brand] ?? formatGroupName(brand.replaceAll("-", "_"));
    return brand === handle ? label : `${label} (${handle})`;
  }
  const handles = [...new Set([...(provider ? [provider] : []), ...enabledProviders])];
  return <div className="grid grid-cols-2 gap-2">
      <ComboboxInput label="Provider" value={provider} allowFreeText={false} placeholder="Select provider"
        items={handles.map(handle => ({ value: handle, label: `${providerLabel(handle)}${enabledProviders.includes(handle) ? "" : " (disabled)"}` }))}
        onChange={value => { if (enabledProviders.includes(value)) onProviderChange(value); }} />
      <ComboboxInput label="Model" value={draft} allowFreeText disabled={!provider}
        items={(directory?.models ?? []).map(row => ({ value: row.id, label: row.name ?? row.id }))}
        placeholder={loading ? "Fetching models..." : "Select or enter model"}
        onChange={value => { setDraft(value); if (directory?.models.some(row => row.id === value)) onModelChange(value); }}
        onBlur={() => { if (draft.trim() !== model) onModelChange(draft.trim()); }} />
  </div>;
}
