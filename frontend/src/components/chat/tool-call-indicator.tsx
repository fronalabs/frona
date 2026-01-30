import { CheckIcon } from "@heroicons/react/16/solid";
import type { ToolCallStatus } from "@/lib/types";

export function ToolCallIndicator({ toolCall }: { toolCall: ToolCallStatus }) {
  const label =
    toolCall.description || toolCall.name.replace(/_/g, " ");

  if (toolCall.status === "running") {
    return (
      <div className="flex items-center gap-1.5">
        <div className="h-3 w-3 shrink-0 animate-spin rounded-full border-[1.5px] border-text-tertiary border-t-transparent" />
        <span className="text-xs text-text-secondary">{label}</span>
      </div>
    );
  }

  return (
    <div className="flex items-center gap-1.5">
      <CheckIcon className="h-3 w-3 shrink-0 text-text-tertiary" />
      <span className="text-xs text-text-tertiary">{label}</span>
    </div>
  );
}
