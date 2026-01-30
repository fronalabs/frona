"use client";

import { ChatBubbleLeftRightIcon, ClipboardDocumentListIcon, CpuChipIcon } from "@heroicons/react/24/outline";
import { useNavigation } from "@/lib/navigation-context";

const tabs = [
  { id: "chat" as const, label: "Chat", Icon: ChatBubbleLeftRightIcon },
  { id: "tasks" as const, label: "Tasks", Icon: ClipboardDocumentListIcon },
  { id: "agents" as const, label: "Agents", Icon: CpuChipIcon },
];

export function TabBar() {
  const { activeTab, setActiveTab } = useNavigation();

  return (
    <div className="flex border-b border-border">
      {tabs.map(({ id, label, Icon }) => (
        <button
          key={id}
          onClick={() => setActiveTab(id)}
          className={`flex-1 flex items-center justify-center gap-1.5 py-2.5 text-xs font-medium transition ${
            activeTab === id
              ? "text-text-primary border-b-2 border-text-primary"
              : "text-text-tertiary hover:text-text-secondary"
          }`}
        >
          <Icon className="h-4 w-4" />
          {label}
        </button>
      ))}
    </div>
  );
}
