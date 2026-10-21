"use client";

import { useState, useRef, useEffect } from "react";
import {
  EllipsisVerticalIcon,
  TrashIcon,
  XCircleIcon,
} from "@heroicons/react/24/outline";

interface TaskActionsProps {
  canCancel: boolean;
  canDelete: boolean;
  onCancel: () => void;
  onDelete: () => void;
}

export function TaskActions({
  canCancel,
  canDelete,
  onCancel,
  onDelete,
}: TaskActionsProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handleClick(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [open]);

  if (!canCancel && !canDelete) return null;

  return (
    <div ref={ref} className="relative">
      <button
        aria-label="Task actions"
        aria-expanded={open}
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        className="rounded p-0.5 text-text-tertiary hover:text-text-primary transition row-action group-hover:opacity-100 group-focus-within:opacity-100 focus:opacity-100"
      >
        <EllipsisVerticalIcon className="h-5 w-5" />
      </button>
      {open && (
        <div className="absolute right-0 top-full z-50 mt-1 w-36 rounded-lg border border-border bg-surface shadow-lg py-1">
          {canCancel && (
            <button
              onClick={(e) => {
                e.stopPropagation();
                setOpen(false);
                onCancel();
              }}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
            >
              <XCircleIcon className="h-4 w-4" />
              Cancel
            </button>
          )}
          {canDelete && (
            <button
              onClick={(e) => {
                e.stopPropagation();
                setOpen(false);
                onDelete();
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
  );
}
