"use client";

import { useMemo } from "react";
import type {
  Unstable_TriggerAdapter,
  Unstable_TriggerCategory,
  Unstable_TriggerItem,
  Unstable_DirectiveFormatter,
  Unstable_DirectiveSegment,
} from "@assistant-ui/core";

import type { CommandsResponse } from "@/lib/api-client";
import { CLIENT_BUILTINS } from "@/components/chat/client-commands";

/** Discovery endpoint marks agent entries with `argument_hint: "[prompt]"`. */
function isAgentCommand(c: { argument_hint?: string }): boolean {
  return c.argument_hint === "[prompt]";
}

/** `label` is what the dropdown shows; `metadata.name` is the wire-format
 *  value the formatter serializes back to text. The two diverge for agents
 *  (label = display name, name = handle). */
function buildItems(
  commands: CommandsResponse | null,
  mode: "slash" | "at",
): readonly Unstable_TriggerItem[] {
  if (mode === "at") {
    return (commands?.commands ?? [])
      .filter(isAgentCommand)
      .map<Unstable_TriggerItem>((c) => ({
        id: `agent:${c.name}`,
        type: "agent",
        label: `@${c.display_name}`,
        description: c.description,
        metadata: { name: c.name },
      }));
  }

  const items: Unstable_TriggerItem[] = [];

  for (const b of CLIENT_BUILTINS) {
    items.push({
      id: `builtin:${b.name}`,
      type: "client-builtin",
      label: `/${b.name}`,
      description: b.description,
      metadata: { name: b.name },
    });
  }

  for (const c of commands?.commands ?? []) {
    const agent = isAgentCommand(c);
    items.push({
      id: agent ? `agent:${c.name}` : `command:${c.name}`,
      type: agent ? "agent" : "server-command",
      label: `/${c.display_name}`,
      description: c.description,
      metadata: { name: c.name },
    });
  }

  for (const s of commands?.skills ?? []) {
    items.push({
      id: `skill:${s.name}`,
      type: "skill",
      label: `/${s.name}`,
      description: s.description,
      metadata: { name: s.name },
    });
  }

  return items;
}

/** Empty — keeps the popover in search mode from the first keystroke. */
const NO_CATEGORIES: readonly Unstable_TriggerCategory[] = [];

export function useFronaTriggerAdapter(
  commands: CommandsResponse | null,
  mode: "slash" | "at",
): Unstable_TriggerAdapter {
  return useMemo<Unstable_TriggerAdapter>(() => {
    const items = buildItems(commands, mode);
    return {
      categories: () => NO_CATEGORIES,
      categoryItems: () => items,
      search: (query) => {
        if (!query) return items;
        const q = query.toLowerCase();
        return items.filter((i) => {
          const handle = typeof i.metadata?.["name"] === "string"
            ? (i.metadata["name"] as string).toLowerCase()
            : "";
          return (
            i.label.toLowerCase().includes(q) ||
            handle.includes(q) ||
            i.id.toLowerCase().includes(q)
          );
        });
      },
    };
  }, [commands, mode]);
}

/** Assistant UI inserts the separator after the chip; serialize only the directive. */
function makeFormatter(prefix: string): Unstable_DirectiveFormatter {
  return {
    serialize: (item: Unstable_TriggerItem) => {
      const name = (item.metadata?.["name"] as string | undefined) ?? item.id;
      return `${prefix}${name}`;
    },
    parse: (text: string): readonly Unstable_DirectiveSegment[] => [
      { kind: "text", text },
    ],
  };
}

export const fronaSlashFormatter = makeFormatter("/");
export const fronaAtFormatter = makeFormatter("@");
