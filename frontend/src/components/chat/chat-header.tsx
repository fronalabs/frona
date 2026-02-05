"use client";

import { useState, useRef, useEffect } from "react";
import { useRouter } from "next/navigation";
import {
  EllipsisVerticalIcon,
  ArchiveBoxIcon,
  ArchiveBoxXMarkIcon,
  TrashIcon,
} from "@heroicons/react/24/outline";
import { useSession } from "@/lib/session-context";
import { useNavigation, neighborRoute } from "@/lib/navigation-context";
import { agentDisplayName } from "@/lib/types";
import { DeleteConfirmDialog } from "@/components/nav/delete-confirm-dialog";

export function ChatHeader() {
  const { activeChat, pendingAgentId } = useSession();
  const { agents, standaloneChats, archivedChats, archiveChat, unarchiveChat, deleteChat } = useNavigation();
  const router = useRouter();
  const [menuOpen, setMenuOpen] = useState(false);
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  const agentId = activeChat?.agent_id ?? pendingAgentId;

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

  if (!agentId) return null;

  const agent = agents.find((a) => a.id === agentId);
  const agentName = agentDisplayName(agentId, agent?.name);
  const isArchived = !!activeChat?.archived_at;

  const handleArchive = async () => {
    if (!activeChat) return;
    setMenuOpen(false);
    await archiveChat(activeChat.id);
    router.push("/chat?agent=system");
  };

  const handleUnarchive = async () => {
    if (!activeChat) return;
    setMenuOpen(false);
    await unarchiveChat(activeChat.id);
  };

  const handleDelete = async () => {
    if (!activeChat) return;
    const next =
      neighborRoute(standaloneChats, activeChat.id, (id) => `/chat?id=${id}`) ??
      neighborRoute(archivedChats, activeChat.id, (id) => `/chat?id=${id}`);
    setShowDeleteDialog(false);
    await deleteChat(activeChat.id);
    router.push(next ?? "/chat?agent=system");
  };

  return (
    <div className="flex items-center border-b border-border px-6 py-3">
      <div className="flex-1 min-w-0">
        <h2 className="text-sm font-semibold text-text-primary">
          {activeChat?.title ?? "New chat"}
        </h2>
        <p className="text-xs text-text-tertiary">{agentName}</p>
      </div>
      {activeChat && (
        <div ref={menuRef} className="relative ml-2">
          <button
            onClick={() => setMenuOpen((v) => !v)}
            className="rounded p-1 text-text-tertiary hover:text-text-primary transition"
          >
            <EllipsisVerticalIcon className="h-6 w-6" />
          </button>
          {menuOpen && (
            <div className="absolute right-0 top-full z-50 mt-1 w-36 rounded-lg border border-border bg-surface shadow-lg py-1">
              {isArchived ? (
                <button
                  onClick={handleUnarchive}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
                >
                  <ArchiveBoxXMarkIcon className="h-4 w-4" />
                  Unarchive
                </button>
              ) : (
                <button
                  onClick={handleArchive}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
                >
                  <ArchiveBoxIcon className="h-4 w-4" />
                  Archive
                </button>
              )}
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
            </div>
          )}
        </div>
      )}
      <DeleteConfirmDialog
        open={showDeleteDialog}
        onCancel={() => setShowDeleteDialog(false)}
        onConfirm={handleDelete}
      />
    </div>
  );
}
