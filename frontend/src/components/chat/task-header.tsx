"use client";

import { useState, useRef, useEffect } from "react";
import { useRouter } from "next/navigation";
import {
  EllipsisVerticalIcon,
  TrashIcon,
  XCircleIcon,
} from "@heroicons/react/24/outline";
import { cancelTask } from "@/lib/api-client";
import { useSession } from "@/lib/session-context";
import { useNavigation, neighborRoute } from "@/lib/navigation-context";
import { agentDisplayName } from "@/lib/types";
import { DeleteConfirmDialog } from "@/components/nav/delete-confirm-dialog";

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

export function TaskHeader() {
  const { activeTask } = useSession();
  const { agents, tasks, deleteTask } = useNavigation();
  const router = useRouter();
  const [menuOpen, setMenuOpen] = useState(false);
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    function handleClick(e: MouseEvent) {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [menuOpen]);

  if (!activeTask) return null;

  const agent = agents.find((a) => a.id === activeTask.agent_id);
  const agentName = agentDisplayName(activeTask.agent_id, agent?.name);
  const colorClass = statusColors[activeTask.status] ?? "bg-surface-tertiary text-text-secondary";
  const label = statusLabels[activeTask.status] ?? activeTask.status;
  const canCancel = activeStatuses.has(activeTask.status);
  const canDelete = terminalStatuses.has(activeTask.status);

  const handleCancel = async () => {
    setMenuOpen(false);
    try {
      await cancelTask(activeTask.id);
    } catch {
      // ignore
    }
  };

  const handleDelete = async () => {
    const next = neighborRoute(tasks, activeTask.id, (id) => `/chat?task=${id}`);
    setShowDeleteDialog(false);
    await deleteTask(activeTask.id);
    router.push(next ?? "/chat?agent=system");
  };

  return (
    <div className="flex items-center border-b border-border px-6 py-3">
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2">
          <h2 className="text-sm font-semibold text-text-primary">{activeTask.title}</h2>
          <span className={`rounded-full px-2 py-0.5 text-[10px] font-medium ${colorClass}`}>
            {label}
          </span>
        </div>
        <p className="text-xs text-text-tertiary">{agentName}</p>
      </div>
      {(canCancel || canDelete) && (
        <div ref={menuRef} className="relative ml-2">
          <button
            onClick={() => setMenuOpen((v) => !v)}
            className="rounded p-1 text-text-tertiary hover:text-text-primary transition"
          >
            <EllipsisVerticalIcon className="h-6 w-6" />
          </button>
          {menuOpen && (
            <div className="absolute right-0 top-full z-50 mt-1 w-36 rounded-lg border border-border bg-surface shadow-lg py-1">
              {canCancel && (
                <button
                  onClick={handleCancel}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
                >
                  <XCircleIcon className="h-4 w-4" />
                  Cancel
                </button>
              )}
              {canDelete && (
                <button
                  onClick={() => {
                    setMenuOpen(false);
                    setShowDeleteDialog(true);
                  }}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
                >
                  <TrashIcon className="h-4 w-4" />
                  Delete
                </button>
              )}
            </div>
          )}
        </div>
      )}
      <DeleteConfirmDialog
        open={showDeleteDialog}
        onCancel={() => setShowDeleteDialog(false)}
        onConfirm={handleDelete}
        title="Delete task?"
        message="This will permanently delete this task and its conversation. This action cannot be undone."
      />
    </div>
  );
}
