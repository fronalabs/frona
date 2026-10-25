"use client";

import { useState } from "react";
import { useRouter, useSearchParams, usePathname } from "next/navigation";
import {
  FolderPlusIcon,
  FolderIcon,
  PlusIcon,
  ArchiveBoxIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  EllipsisVerticalIcon,
  PencilIcon,
  TrashIcon,
} from "@heroicons/react/24/outline";
import { api } from "@/lib/api-client";
import { useNavigation, neighborRoute } from "@/lib/navigation-context";
import { useSession } from "@/lib/session-context";
import { ChatActions } from "./chat-actions";
import { DeleteConfirmDialog } from "./delete-confirm-dialog";
import type { SpaceResponse, SpaceWithChats } from "@/lib/types";

export function ChatsTab() {
  const {
    spaces,
    standaloneChats,
    archivedChats,
    showArchived,
    setShowArchived,
    refresh,
    archiveChat,
    unarchiveChat,
    deleteChat,
    setMobileNavOpen,
  } = useNavigation();
  const { activeChatId, activeChat, setActiveChat } = useSession();
  const selectedChatId = activeChatId ?? activeChat?.id ?? null;
  const router = useRouter();
  const searchParams = useSearchParams();
  const pathname = usePathname();
  const activeSpaceId = pathname === "/space"
    ? searchParams.get("id")
    : activeChat?.space_id ?? null;
  const [creatingSpace, setCreatingSpace] = useState(false);
  const [spaceName, setSpaceName] = useState("");
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);
  const [spaceMenu, setSpaceMenu] = useState<string | null>(null);
  const [renameTarget, setRenameTarget] = useState<SpaceWithChats | null>(null);
  const [renameName, setRenameName] = useState("");
  const [deleteSpaceTarget, setDeleteSpaceTarget] = useState<SpaceWithChats | null>(null);
  const [spaceActionPending, setSpaceActionPending] = useState(false);
  const [spaceError, setSpaceError] = useState<string | null>(null);

  const navigate = (href: string) => {
    // Dismiss on selection, including the current route, without waiting for navigation.
    setMobileNavOpen(false);
    router.push(href);
  };

  const handleNewChat = () => {
    setActiveChat(null);
    navigate("/chat");
  };

  const handleCreateSpace = async (e: React.FormEvent) => {
    e.preventDefault();
    const name = spaceName.trim();
    if (!name) return;
    await api.post<SpaceResponse>("/api/spaces", { name });
    setSpaceName("");
    setCreatingSpace(false);
    refresh();
  };

  const handleArchive = async (chatId: string) => {
    await archiveChat(chatId);
    if (selectedChatId === chatId) {
      router.push("/chat");
    }
  };

  const handleUnarchive = async (chatId: string) => {
    await unarchiveChat(chatId);
  };

  const handleDeleteConfirm = async () => {
    if (!deleteTarget) return;
    const wasActive = selectedChatId === deleteTarget;
    const next =
      neighborRoute(standaloneChats, deleteTarget, (id) => `/chat?id=${id}`) ??
      neighborRoute(archivedChats, deleteTarget, (id) => `/chat?id=${id}`);
    await deleteChat(deleteTarget);
    setDeleteTarget(null);
    if (wasActive) {
      router.push(next ?? "/chat");
    }
  };

  const handleRenameSpace = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!renameTarget || !renameName.trim()) return;
    setSpaceActionPending(true);
    setSpaceError(null);
    try {
      await api.put<SpaceResponse>(`/api/spaces/${renameTarget.id}`, {
        name: renameName.trim(),
      });
      setRenameTarget(null);
      await refresh();
    } catch (error) {
      setSpaceError(error instanceof Error ? error.message : "Failed to rename space");
    } finally {
      setSpaceActionPending(false);
    }
  };

  const handleDeleteSpace = async () => {
    if (!deleteSpaceTarget) return;
    setSpaceActionPending(true);
    setSpaceError(null);
    try {
      const deletingCurrentSpace = activeSpaceId === deleteSpaceTarget.id;
      await api.delete(`/api/spaces/${deleteSpaceTarget.id}`);
      setDeleteSpaceTarget(null);
      if (deletingCurrentSpace) {
        setActiveChat(null);
        router.push("/chat");
      }
      await refresh();
    } catch (error) {
      setSpaceError(error instanceof Error ? error.message : "Failed to delete space");
    } finally {
      setSpaceActionPending(false);
    }
  };

  return (
    <div className="space-y-1 p-2">
      <div className="flex items-center justify-between px-2 pb-1">
        <span className="text-[10px] font-semibold uppercase tracking-wider text-text-tertiary">
          Spaces
        </span>
        <button
          onClick={() => setCreatingSpace((v) => !v)}
          className="rounded p-0.5 text-text-tertiary hover:text-text-primary transition"
          title="New space"
        >
          <FolderPlusIcon className="h-3.5 w-3.5" />
        </button>
      </div>

      {spaceError && (
        <p className="px-2 py-1 text-xs text-danger">{spaceError}</p>
      )}

      {creatingSpace && (
        <form onSubmit={handleCreateSpace} className="px-2 pb-1">
          <input
            autoFocus
            value={spaceName}
            onChange={(e) => setSpaceName(e.target.value)}
            onBlur={() => {
              if (!spaceName.trim()) setCreatingSpace(false);
            }}
            placeholder="Space name..."
            className="w-full rounded-lg border border-border bg-surface px-2 py-1 text-sm text-text-primary placeholder:text-text-tertiary focus:outline-none focus:border-text-secondary"
          />
        </form>
      )}

      {spaces.map((space) => (
        <div
          key={space.id}
          className={`group relative flex w-full items-center rounded-lg text-sm font-medium transition ${
            activeSpaceId === space.id
              ? "bg-surface-tertiary text-text-primary"
              : "text-text-primary hover:bg-surface-secondary"
          }`}
        >
          <button
            onClick={() => navigate(`/space?id=${space.id}`)}
            className="flex min-w-0 flex-1 items-center gap-1 px-3 py-2 text-left"
          >
            <FolderIcon className="h-4 w-4 shrink-0 text-text-tertiary" />
            <span className="truncate">{space.name}</span>
            <span className="ml-auto text-[10px] text-text-tertiary">{space.chats.length}</span>
          </button>
          <button
            onClick={() => setSpaceMenu((id) => (id === space.id ? null : space.id))}
            className="mr-1 rounded p-0.5 text-text-tertiary row-action transition hover:text-text-primary group-hover:opacity-100 group-focus-within:opacity-100 focus:opacity-100"
            aria-label={`Actions for ${space.name}`}
          >
            <EllipsisVerticalIcon className="h-5 w-5" />
          </button>
          {spaceMenu === space.id && (
            <div className="absolute right-1 top-full z-50 mt-1 w-32 rounded-lg border border-border bg-surface py-1 shadow-lg">
              <button
                onClick={() => {
                  setSpaceMenu(null);
                  setRenameTarget(space);
                  setRenameName(space.name);
                }}
                className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary"
              >
                <PencilIcon className="h-4 w-4" /> Rename
              </button>
              <button
                onClick={() => {
                  setSpaceMenu(null);
                  setDeleteSpaceTarget(space);
                }}
                className="flex w-full items-center gap-2 px-3 py-1.5 text-sm text-danger hover:bg-surface-secondary"
              >
                <TrashIcon className="h-4 w-4" /> Delete
              </button>
            </div>
          )}
        </div>
      ))}

      {renameTarget && (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
          <div className="absolute inset-0 bg-black/50" onClick={() => setRenameTarget(null)} />
          <form onSubmit={handleRenameSpace} className="relative w-full max-w-sm rounded-xl border border-border bg-surface p-6 shadow-xl mx-4">
            <h3 className="text-sm font-semibold text-text-primary">Rename space</h3>
            <input
              autoFocus
              value={renameName}
              onChange={(e) => setRenameName(e.target.value)}
              className="mt-3 w-full rounded-lg border border-border bg-surface-secondary px-3 py-2 text-sm text-text-primary focus:outline-none focus:border-text-secondary"
            />
            <div className="mt-4 flex justify-end gap-2">
              <button type="button" onClick={() => setRenameTarget(null)} disabled={spaceActionPending} className="rounded-lg px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary">Cancel</button>
              <button type="submit" disabled={spaceActionPending || !renameName.trim()} className="rounded-lg bg-accent px-3 py-1.5 text-sm text-surface disabled:opacity-50">Rename</button>
            </div>
          </form>
        </div>
      )}

      <DeleteConfirmDialog
        open={deleteSpaceTarget !== null}
        onCancel={() => setDeleteSpaceTarget(null)}
        onConfirm={handleDeleteSpace}
        title={`Delete ${deleteSpaceTarget?.name ?? "space"}?`}
        message={
          <div className="space-y-3">
            <p>
              This will permanently delete the space, its chats, and all messages.
              This action cannot be undone.
            </p>
            {(deleteSpaceTarget?.chat_count ?? 0) > 0 && (
              <div className="rounded-lg border border-danger/30 bg-danger/10 px-3 py-2 font-medium text-danger">
                {deleteSpaceTarget?.chat_count} {deleteSpaceTarget?.chat_count === 1 ? "chat" : "chats"} will be deleted
              </div>
            )}
          </div>
        }
        confirmLabel="Delete space"
        confirming={spaceActionPending}
        confirmingLabel="Deleting..."
      />

      {standaloneChats.length > 0 && (
        <div className="pt-2">
          <div className="flex items-center justify-between px-2 pb-1">
            <span className="text-[10px] font-semibold uppercase tracking-wider text-text-tertiary">
              Chats
            </span>
            <button
              onClick={handleNewChat}
              className="rounded p-0.5 text-text-tertiary hover:text-text-primary transition"
              title="New chat"
            >
              <PlusIcon className="h-3.5 w-3.5" />
            </button>
          </div>
          {standaloneChats.map((chat) => (
            <div
              key={chat.id}
              className={`group flex items-center rounded-lg pr-1 transition ${
                selectedChatId === chat.id
                  ? "bg-surface-tertiary text-text-primary"
                  : "text-text-secondary hover:bg-surface-secondary"
              }`}
            >
              <button
                onClick={() => navigate(`/chat?id=${chat.id}`)}
                className="flex-1 min-w-0 px-3 py-2 text-left text-sm truncate"
              >
                {chat.title ?? "New chat"}
              </button>
              <ChatActions
                isArchived={false}
                onArchive={() => handleArchive(chat.id)}
                onUnarchive={() => {}}
                onDelete={() => setDeleteTarget(chat.id)}
              />
            </div>
          ))}
        </div>
      )}

      <div className="pt-2">
        <button
          onClick={() => setShowArchived(!showArchived)}
          className="flex w-full items-center gap-1.5 px-2 pb-1 text-[10px] font-semibold uppercase tracking-wider text-text-tertiary hover:text-text-secondary transition"
        >
          <ArchiveBoxIcon className="h-3 w-3" />
          Archived
          {showArchived ? (
            <ChevronDownIcon className="ml-auto h-3 w-3" />
          ) : (
            <ChevronRightIcon className="ml-auto h-3 w-3" />
          )}
        </button>
        {showArchived &&
          archivedChats.map((chat) => (
            <div
              key={chat.id}
              className={`group flex items-center rounded-lg pr-1 transition ${
                selectedChatId === chat.id
                  ? "bg-surface-tertiary text-text-primary"
                  : "text-text-secondary hover:bg-surface-secondary"
              }`}
            >
              <button
                onClick={() => navigate(`/chat?id=${chat.id}`)}
                className="flex-1 min-w-0 px-3 py-2 text-left text-sm truncate"
              >
                {chat.title ?? "New chat"}
              </button>
              <ChatActions
                isArchived
                onArchive={() => {}}
                onUnarchive={() => handleUnarchive(chat.id)}
                onDelete={() => setDeleteTarget(chat.id)}
              />
            </div>
          ))}
        {showArchived && archivedChats.length === 0 && (
          <p className="px-3 py-2 text-xs text-text-tertiary">
            No archived chats
          </p>
        )}
      </div>

      {spaces.length === 0 && standaloneChats.length === 0 && !creatingSpace && (
        <p className="px-2 py-4 text-center text-xs text-text-tertiary">
          No chats yet. Start a new conversation!
        </p>
      )}

      <DeleteConfirmDialog
        open={deleteTarget !== null}
        onCancel={() => setDeleteTarget(null)}
        onConfirm={handleDeleteConfirm}
      />
    </div>
  );
}
