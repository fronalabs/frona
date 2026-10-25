"use client";

import type { TaskResponse } from "@/lib/types";
import { cancelTask } from "@/lib/api-client";
import { useSession } from "@/lib/session-context";
import { useNavigation, neighborRoute } from "@/lib/navigation-context";
import { useRouter } from "next/navigation";
import { useState } from "react";
import { ArrowPathIcon } from "@heroicons/react/24/outline";
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
  const { tasks, deleteTask, setMobileNavOpen } = useNavigation();
  const router = useRouter();
  const isCron = task.kind.type === "Cron";
  const isCronActive = isCron && task.status !== "cancelled";
  const colorClass = isCronActive
    ? "bg-info-bg text-info-text"
    : statusColors[task.status] ?? "bg-surface-tertiary text-text-secondary";
  const label = isCronActive ? "Recurring" : statusLabels[task.status] ?? task.status;
  // Crons never reach terminal status on their own; allow cancel until then.
  const canCancel = isCron ? task.status !== "cancelled" : activeStatuses.has(task.status);
  const canDelete = terminalStatuses.has(task.status);
  const isActive = activeTaskId === task.id;

  const cronNextRun = (() => {
    if (task.kind.type !== "Cron" || !task.kind.next_run_at) return null;
    try {
      const date = new Date(task.kind.next_run_at);
      const tz = task.kind.timezone ?? undefined;
      return new Intl.DateTimeFormat(undefined, {
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
        timeZone: tz,
        timeZoneName: "short",
      }).format(date);
    } catch {
      return null;
    }
  })();

  const handleCancel = async () => {
    try {
      await cancelTask(task.id);
    } catch {}
  };

  const handleDeleteConfirm = async () => {
    const next = neighborRoute(tasks, task.id, (id) => `/chat?task=${id}`);
    setShowDeleteDialog(false);
    await deleteTask(task.id);
    if (isActive) {
      router.push(next ?? "/chat");
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
          onClick={() => {
            setMobileNavOpen(false);
            router.push(`/chat?task=${task.id}`);
          }}
          className="flex-1 min-w-0 px-3 py-2 text-left text-sm"
          title={isCron && cronNextRun ? `Next run: ${cronNextRun}` : undefined}
        >
          <div className="flex items-center gap-1.5 min-w-0">
            {isCron && <ArrowPathIcon className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />}
            <span className="truncate">{task.title}</span>
          </div>
          {isCron && cronNextRun && (
            <div className="text-[10px] text-text-tertiary truncate mt-0.5">Next: {cronNextRun}</div>
          )}
        </button>
        <span className={`rounded-full px-2 py-0.5 text-[10px] font-medium shrink-0 ${colorClass}`}>
          {isCron ? "Recurring" : label}
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
