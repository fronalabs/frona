"use client";

import type { SearchConfig } from "@/lib/config-types";
import { TextInput, SelectInput, SectionHeader, SectionPanel } from "@/components/settings/field";
import { MagnifyingGlassIcon } from "@heroicons/react/24/outline";

interface SearchSectionProps {
  search: SearchConfig;
  onChange: (search: SearchConfig) => void;
}

const searchProviders = [
  { value: "searxng", label: "SearXNG" },
  { value: "tavily", label: "Tavily" },
  { value: "brave", label: "Brave" },
  { value: "keenable", label: "Keenable" },
];

function inferProvider(search: SearchConfig): string | null {
  if (search.provider) return search.provider;
  if (search.searxng_base_url) return "searxng";
  return null;
}

export function SearchSection({ search, onChange }: SearchSectionProps) {
  const effectiveProvider = inferProvider(search);

  return (
    <div>
      <SectionHeader title="Web Search" description="Search provider for agent web search tools" icon={MagnifyingGlassIcon} />
      <SectionPanel>

      <SelectInput
        label="Provider"
        description="Select a web search provider"
        value={effectiveProvider}
        onChange={(provider) => onChange({ ...search, provider })}
        options={searchProviders}
      />

      {effectiveProvider === "searxng" && (
        <TextInput
          label="SearXNG Base URL"
          description="Base URL of the SearXNG instance"
          value={search.searxng_base_url}
          onChange={(searxng_base_url) => onChange({ ...search, searxng_base_url })}
          placeholder="https://search.example.com"
        />
      )}
      </SectionPanel>
    </div>
  );
}
