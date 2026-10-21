"use client";

import { SectionHeader, SectionPanel } from "@/components/settings/field";
import { DocumentTextIcon } from "@heroicons/react/24/outline";

interface InstructionsSectionProps {
  prompt: string;
  onPromptChange: (prompt: string) => void;
}

export function InstructionsSection({ prompt, onPromptChange }: InstructionsSectionProps) {
  return (
    <div className="flex min-h-0 flex-col h-full">
      <SectionHeader title="Prompt" description="System prompt sent at the beginning of every conversation" icon={DocumentTextIcon} />
      <SectionPanel className="flex-1 min-h-0 flex flex-col">
        <p className="text-sm text-text-tertiary">
          The system prompt defines how this agent behaves, what it knows, and how it responds.{" "}
          <a
            href="https://docs.anthropic.com/en/docs/build-with-claude/prompt-engineering/overview"
            target="_blank"
            rel="noopener noreferrer"
            className="text-accent hover:underline"
          >
            Learn more about prompt engineering
          </a>
        </p>
        <textarea
          value={prompt}
          onChange={(e) => onPromptChange(e.target.value)}
          aria-label="System prompt"
          className="flex-1 min-h-32 w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary font-mono placeholder:text-text-tertiary focus:border-accent focus:outline-none resize-none"
        />
      </SectionPanel>
    </div>
  );
}
