"use client";

import type { TaskResponse } from "@/lib/types";

const statusColors: Record<string, string> = {
  pending: "bg-yellow-100 text-yellow-800",
  inprogress: "bg-blue-100 text-blue-800",
  completed: "bg-green-100 text-green-800",
  failed: "bg-red-100 text-red-800",
};

const statusLabels: Record<string, string> = {
  pending: "Pending",
  inprogress: "In Progress",
  completed: "Done",
  failed: "Failed",
};

interface TaskItemProps {
  task: TaskResponse;
}

export function TaskItem({ task }: TaskItemProps) {
  const colorClass = statusColors[task.status] ?? "bg-surface-tertiary text-text-secondary";
  const label = statusLabels[task.status] ?? task.status;

  return (
    <div className="flex items-center justify-between rounded-lg px-3 py-2 text-sm hover:bg-surface-secondary transition">
      <span className="truncate text-text-primary">{task.title}</span>
      <span className={`ml-2 shrink-0 rounded-full px-2 py-0.5 text-[10px] font-medium ${colorClass}`}>
        {label}
      </span>
    </div>
  );
}
