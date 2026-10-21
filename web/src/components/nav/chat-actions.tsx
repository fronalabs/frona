"use client";

import { useState, useRef, useEffect } from "react";
import {
  EllipsisVerticalIcon,
  ArchiveBoxIcon,
  ArchiveBoxXMarkIcon,
  TrashIcon,
} from "@heroicons/react/24/outline";

interface ChatActionsProps {
  isArchived: boolean;
  onArchive: () => void;
  onUnarchive: () => void;
  onDelete: () => void;
}

export function ChatActions({
  isArchived,
  onArchive,
  onUnarchive,
  onDelete,
}: ChatActionsProps) {
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

  return (
    <div ref={ref} className="relative">
      <button
        aria-label="Chat actions"
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
          {isArchived ? (
            <button
              onClick={(e) => {
                e.stopPropagation();
                setOpen(false);
                onUnarchive();
              }}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
            >
              <ArchiveBoxXMarkIcon className="h-4 w-4" />
              Unarchive
            </button>
          ) : (
            <button
              onClick={(e) => {
                e.stopPropagation();
                setOpen(false);
                onArchive();
              }}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
            >
              <ArchiveBoxIcon className="h-4 w-4" />
              Archive
            </button>
          )}
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
        </div>
      )}
    </div>
  );
}
