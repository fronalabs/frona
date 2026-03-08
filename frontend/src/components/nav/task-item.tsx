"use client";

import type { TaskResponse } from "@/lib/types";
import { cancelTask } from "@/lib/api-client";
import { useSession } from "@/lib/session-context";
import { useNavigation, neighborRoute } from "@/lib/navigation-context";
import { useRouter } from "next/navigation";
import { useState } from "react";
import { TaskActions } from "./task-actions";
import { DeleteConfirmDialog } from "./delete-confirm-dialog";

const statusColors: Record<string, string> = {
  pending: "bg-warning-bg text-warning-text",
  inprogress: "bg-info-bg text-info-text",
  completed: "bg-success-bg text-success-text",
  failed: "bg-danger-bg text-danger-text",
  cancelled: "bg-surface-tertiary text-text-secondary",
};

const statusLabels: Record<string, string> = {
  pending: "Pending",
  inprogress: "In Progress",
  completed: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
};

const terminalStatuses = new Set(["completed", "failed", "cancelled"]);
const activeStatuses = new Set(["pending", "inprogress"]);

interface TaskItemProps {
  task: TaskResponse;
}

export function TaskItem({ task }: TaskItemProps) {
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const { activeTaskId } = useSession();
  const { tasks, deleteTask } = useNavigation();
  const router = useRouter();
  const colorClass = statusColors[task.status] ?? "bg-surface-tertiary text-text-secondary";
  const label = statusLabels[task.status] ?? task.status;
  const canCancel = activeStatuses.has(task.status);
  const canDelete = terminalStatuses.has(task.status);
  const isActive = activeTaskId === task.id;

  const handleCancel = async () => {
    try {
      await cancelTask(task.id);
    } catch {
      // ignore
    }
  };

  const handleDeleteConfirm = async () => {
    const next = neighborRoute(tasks, task.id, (id) => `/chat?task=${id}`);
    setShowDeleteDialog(false);
    await deleteTask(task.id);
    if (isActive) {
      router.push(next ?? "/chat?agent=system");
    }
  };

  return (
    <>
      <div
        className={`group flex items-center rounded-lg pr-1 transition ${
          isActive
            ? "bg-surface-tertiary text-text-primary"
            : "text-text-secondary hover:bg-surface-secondary"
        }`}
      >
        <button
          onClick={() => router.push(`/chat?task=${task.id}`)}
          className="flex-1 min-w-0 px-3 py-2 text-left text-sm truncate"
        >
          {task.title}
        </button>
        <span className={`rounded-full px-2 py-0.5 text-[10px] font-medium shrink-0 ${colorClass}`}>
          {label}
        </span>
        <TaskActions
          canCancel={canCancel}
          canDelete={canDelete}
          onCancel={handleCancel}
          onDelete={() => setShowDeleteDialog(true)}
        />
      </div>
      <DeleteConfirmDialog
        open={showDeleteDialog}
        onCancel={() => setShowDeleteDialog(false)}
        onConfirm={handleDeleteConfirm}
        title="Delete task?"
        message="This will permanently delete this task and its conversation. This action cannot be undone."
      />
    </>
  );
}
