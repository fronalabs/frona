"use client";

import { useRef, useState, useEffect, useCallback } from "react";
import { PaperAirplaneIcon, StopIcon, PlusIcon, XMarkIcon } from "@heroicons/react/24/solid";
import { ArrowUpTrayIcon, CloudIcon, FolderOpenIcon } from "@heroicons/react/24/outline";
import { useSession } from "@/lib/session-context";
import { uploadFile } from "@/lib/api-client";
import { AutoResizeTextarea, type AutoResizeTextareaHandle } from "@/components/auto-resize-textarea";
import { FileBrowserModal } from "@/components/chat/file-browser-modal";
import type { Attachment } from "@/lib/types";

interface PendingFile {
  file: File;
  relativePath?: string;
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function MessageInput() {
  const { sendMessage, stopGeneration, sending, activeChatId, pendingAgentId } = useSession();
  const canSend = !!(activeChatId || pendingAgentId);
  const [text, setText] = useState("");
  const [pendingFiles, setPendingFiles] = useState<PendingFile[]>([]);
  const [serverAttachments, setServerAttachments] = useState<Attachment[]>([]);
  const [uploading, setUploading] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [browseOpen, setBrowseOpen] = useState(false);
  const textareaRef = useRef<AutoResizeTextareaHandle>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuButtonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (canSend && !sending) {
      const id = setTimeout(() => textareaRef.current?.focus(), 0);
      return () => clearTimeout(id);
    }
  }, [canSend, sending, pendingAgentId, activeChatId]);

  useEffect(() => {
    if (!menuOpen) return;
    const handleClickOutside = (e: MouseEvent) => {
      if (
        menuRef.current && !menuRef.current.contains(e.target as Node) &&
        menuButtonRef.current && !menuButtonRef.current.contains(e.target as Node)
      ) {
        setMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, [menuOpen]);

  const handleFilesSelected = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files;
    if (!files) return;
    const newFiles: PendingFile[] = Array.from(files).map((file) => ({
      file,
      relativePath: file.webkitRelativePath || undefined,
    }));
    setPendingFiles((prev) => [...prev, ...newFiles]);
    e.target.value = "";
  }, []);

  const removePendingFile = (index: number) => {
    setPendingFiles((prev) => prev.filter((_, i) => i !== index));
  };

  const removeServerAttachment = (index: number) => {
    setServerAttachments((prev) => prev.filter((_, i) => i !== index));
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const content = text.trim();
    const hasFiles = pendingFiles.length > 0 || serverAttachments.length > 0;
    if ((!content && !hasFiles) || !canSend) return;

    setText("");
    textareaRef.current?.resetHeight();
    const filesToUpload = [...pendingFiles];
    const serverFiles = [...serverAttachments];
    setPendingFiles([]);
    setServerAttachments([]);

    let attachments: Attachment[] = [...serverFiles];
    if (filesToUpload.length > 0) {
      setUploading(true);
      try {
        const uploaded = await Promise.all(
          filesToUpload.map((pf) => uploadFile(pf.file, pf.relativePath)),
        );
        attachments = [...attachments, ...uploaded];
      } catch {
        setUploading(false);
        return;
      }
      setUploading(false);
    }

    await sendMessage(content || "See attached files.", attachments.length > 0 ? attachments : undefined);
    requestAnimationFrame(() => textareaRef.current?.focus());
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSubmit(e);
    }
  };

  const isDisabled = !canSend || sending || uploading;

  return (
    <form onSubmit={handleSubmit} className="sticky bottom-0 bg-surface p-4">
      {(pendingFiles.length > 0 || serverAttachments.length > 0) && (
        <div className="flex flex-wrap gap-1.5 mb-2 px-1">
          {pendingFiles.map((pf, i) => (
            <span
              key={`local-${i}`}
              className="inline-flex items-center gap-1 rounded-md bg-surface-tertiary px-2 py-1 text-xs text-text-secondary"
            >
              <span className="max-w-[200px] truncate">{pf.relativePath || pf.file.name}</span>
              <span className="text-text-tertiary">({formatFileSize(pf.file.size)})</span>
              <button
                type="button"
                onClick={() => removePendingFile(i)}
                className="ml-0.5 hover:text-text-primary"
              >
                <XMarkIcon className="h-3 w-3" />
              </button>
            </span>
          ))}
          {serverAttachments.map((att, i) => (
            <span
              key={`server-${i}`}
              className="inline-flex items-center gap-1 rounded-md bg-surface-tertiary px-2 py-1 text-xs text-text-secondary"
            >
              <span className="max-w-[200px] truncate">{att.filename}</span>
              <span className="text-text-tertiary">({formatFileSize(att.size_bytes)})</span>
              <button
                type="button"
                onClick={() => removeServerAttachment(i)}
                className="ml-0.5 hover:text-text-primary"
              >
                <XMarkIcon className="h-3 w-3" />
              </button>
            </span>
          ))}
        </div>
      )}
      <div className="rounded-2xl border border-border bg-surface-secondary p-4 focus-within:border-accent transition-colors">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFilesSelected}
        />
        <AutoResizeTextarea
          ref={textareaRef}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Send a message..."
          className="w-full bg-transparent text-sm leading-5 text-text-primary placeholder:text-text-tertiary focus:outline-none"
          disabled={isDisabled}
        />
        <div className="flex items-center justify-between pt-2">
          <div className="relative">
            <button
              ref={menuButtonRef}
              type="button"
              onClick={() => setMenuOpen((v) => !v)}
              disabled={isDisabled}
              className="rounded-lg p-1 text-text-tertiary hover:text-text-secondary hover:bg-surface-tertiary disabled:opacity-30 transition"
              title="Attach"
            >
              <PlusIcon className="h-5 w-5" />
            </button>
            {menuOpen && (
              <div
                ref={menuRef}
                className="absolute bottom-full left-0 mb-1 w-max rounded-lg border border-border bg-surface-secondary py-1 shadow-lg"
              >
                <button
                  type="button"
                  className="flex w-full items-center gap-2 px-3 py-2 text-sm text-text-secondary hover:bg-surface-tertiary transition-colors"
                  onClick={() => {
                    fileInputRef.current?.click();
                    setMenuOpen(false);
                  }}
                >
                  <ArrowUpTrayIcon className="h-4 w-4" />
                  Upload files
                </button>
                <button
                  type="button"
                  className="flex w-full items-center gap-2 px-3 py-2 text-sm text-text-secondary hover:bg-surface-tertiary transition-colors"
                  onClick={() => {
                    setBrowseOpen(true);
                    setMenuOpen(false);
                  }}
                >
                  <FolderOpenIcon className="h-4 w-4" />
                  Browse files
                </button>
                <button
                  type="button"
                  className="flex w-full items-center gap-2 px-3 py-2 text-sm text-text-tertiary cursor-default"
                  onClick={() => setMenuOpen(false)}
                >
                  <CloudIcon className="h-4 w-4" />
                  Connect to Google Drive
                </button>
              </div>
            )}
          </div>
          {sending ? (
            <button
              type="button"
              onClick={stopGeneration}
              className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary transition"
            >
              <StopIcon className="h-5 w-5" />
            </button>
          ) : (
            <button
              type="submit"
              disabled={(!text.trim() && pendingFiles.length === 0 && serverAttachments.length === 0) || !canSend || uploading}
              className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary disabled:opacity-30 transition"
            >
              <PaperAirplaneIcon className="h-5 w-5" />
            </button>
          )}
        </div>
      </div>
      <FileBrowserModal
        open={browseOpen}
        onClose={() => setBrowseOpen(false)}
        onSelect={(attachments) => setServerAttachments((prev) => [...prev, ...attachments])}
      />
    </form>
  );
}
