"use client";

import { useRouter } from "next/navigation";
import {
  UserCircleIcon,
  MagnifyingGlassIcon,
  CodeBracketIcon,
  BeakerIcon,
} from "@heroicons/react/24/outline";
import { agentDisplayName, type Agent } from "@/lib/types";

function RobotIcon({ className }: { className?: string }) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      className={className}
    >
      <rect x="5" y="7" width="14" height="12" rx="2" />
      <circle cx="9.5" cy="13" r="1.5" />
      <circle cx="14.5" cy="13" r="1.5" />
      <path d="M12 3v4" />
      <circle cx="12" cy="3" r="1" />
      <path d="M2 13h3M19 13h3" />
    </svg>
  );
}

const defaultIcons: Record<string, React.ComponentType<{ className?: string }>> = {
  system: UserCircleIcon,
  researcher: MagnifyingGlassIcon,
  developer: CodeBracketIcon,
  tester: BeakerIcon,
};

interface AgentItemProps {
  agent: Agent;
}

function AgentIcon({ agent }: { agent: Agent }) {
  if (agent.avatar) {
    return (
      <img
        src={agent.avatar}
        alt={agent.name}
        className="h-5 w-5 shrink-0 rounded-full object-cover"
      />
    );
  }

  const Icon = defaultIcons[agent.id] ?? RobotIcon;
  return <Icon className="h-5 w-5 shrink-0 text-text-tertiary" />;
}

export function AgentItem({ agent }: AgentItemProps) {
  const router = useRouter();

  const handleClick = () => {
    router.push(`/chat?agent=${agent.id}`);
  };

  return (
    <button
      onClick={handleClick}
      className="w-full flex items-center gap-2 rounded-lg px-3 py-2 text-left text-sm text-text-secondary hover:bg-surface-secondary transition"
    >
      <AgentIcon agent={agent} />
      <span className="truncate flex-1">{agentDisplayName(agent.id, agent.name)}</span>
      <span
        className={`ml-2 h-2 w-2 shrink-0 rounded-full ${
          agent.enabled ? "bg-green-500" : "bg-text-tertiary"
        }`}
      />
    </button>
  );
}
