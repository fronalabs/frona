"use client";

import { useState, useRef, useEffect } from "react";
import { useRouter } from "next/navigation";
import {
  UserGroupIcon,
  Cog6ToothIcon,
  TrashIcon,
} from "@heroicons/react/24/outline";
import { useNavigation } from "@/lib/navigation-context";
import { agentDisplayName, type Agent } from "@/lib/types";

function AgentIcon({ agent }: { agent: Agent }) {
  if (agent.avatar_url) {
    return (
      <img
        src={agent.avatar_url}
        alt={agent.name}
        className="h-7 w-7 shrink-0 rounded-full object-cover"
      />
    );
  }
  const name = agentDisplayName(agent.id, agent.name);
  return (
    <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-white/10 text-text-secondary">
      {name.charAt(0).toUpperCase()}
    </div>
  );
}

export function AgentDropdown() {
  const router = useRouter();
  const { agents, deleteAgent } = useNavigation();
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    function handleClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        setOpen(false);
        triggerRef.current?.focus();
      }
    }
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [open]);

  const handleDelete = async (e: React.MouseEvent, agent: Agent) => {
    e.stopPropagation();
    if (!confirm(`Delete agent "${agentDisplayName(agent.id, agent.name)}"?`)) return;
    await deleteAgent(agent.id);
  };

  return (
    <div ref={ref} className="relative flex items-center">
      <button
        ref={triggerRef}
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className={`relative flex items-center justify-center h-10 w-10 transition cursor-pointer ${
          open ? "rounded-t-xl rounded-b-none bg-surface-secondary text-text-primary z-[61] border border-border border-b-0" : "rounded-full bg-surface-tertiary text-text-secondary hover:brightness-125"
        }`}
        title="Agents"
      >
        <UserGroupIcon className="h-5 w-5" />
      </button>

      {open && (
        <div className="fixed left-4 right-4 top-14 z-[60] max-h-[70vh] overflow-y-auto md:absolute md:left-auto md:right-0 md:top-full md:w-64 rounded-xl rounded-tr-none border border-border bg-surface-secondary shadow-lg">
          <div className="hidden md:block absolute -top-px right-0 w-[calc(theme(spacing.10)-3px)] h-[2px] bg-surface-secondary z-[60]" />
          <div className="pb-1">
            <div className="flex items-center justify-between px-4 py-2 border-b border-border shrink-0">
              <span className="text-sm font-medium text-text-secondary">Agents</span>
            </div>
            {agents.map((agent) => (
              <div
                key={agent.id}
                className="group flex items-center gap-2 px-4 py-2 hover:bg-surface-tertiary transition cursor-pointer"
                onClick={() => {
                  router.push(`/chat?agent=${agent.id}`);
                  setOpen(false);
                }}
              >
                <AgentIcon agent={agent} />
                <span className="min-w-0 flex-1 truncate text-sm text-text-secondary group-hover:text-text-primary">
                  {agentDisplayName(agent.id, agent.name)}
                </span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    router.push(`/agents?id=${agent.id}`);
                    setOpen(false);
                  }}
                  className="p-1 rounded text-text-tertiary hover:text-text-primary transition row-action group-hover:opacity-100 group-focus-within:opacity-100"
                  title="Configure"
                  aria-label={`Configure ${agentDisplayName(agent.id, agent.name)}`}
                >
                  <Cog6ToothIcon className="h-4 w-4" />
                </button>
                {!agent.is_shared && (
                  <button
                    onClick={(e) => handleDelete(e, agent)}
                    className="p-1 rounded text-text-tertiary hover:text-error-text transition row-action group-hover:opacity-100 group-focus-within:opacity-100"
                    title="Delete"
                    aria-label={`Delete ${agentDisplayName(agent.id, agent.name)}`}
                  >
                    <TrashIcon className="h-4 w-4" />
                  </button>
                )}
              </div>
            ))}
            {agents.length === 0 && (
              <p className="px-3 py-4 text-center text-xs text-text-tertiary">
                No agents configured
              </p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
