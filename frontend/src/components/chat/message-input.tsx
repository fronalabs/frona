"use client";

import { useRef, useState, useEffect } from "react";
import { PaperAirplaneIcon, StopIcon, PaperClipIcon, FolderIcon, XMarkIcon } from "@heroicons/react/24/solid";
import { useSession } from "@/lib/session-context";
import { uploadFile } from "@/lib/api-client";
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
  const [uploading, setUploading] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (canSend && !sending) {
      const id = setTimeout(() => textareaRef.current?.focus(), 0);
      return () => clearTimeout(id);
    }
  }, [canSend, sending, pendingAgentId, activeChatId]);

  const handleFilesSelected = (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files;
    if (!files) return;
    const newFiles: PendingFile[] = Array.from(files).map((file) => ({
      file,
      relativePath: file.webkitRelativePath || undefined,
    }));
    setPendingFiles((prev) => [...prev, ...newFiles]);
    e.target.value = "";
  };

  const removePendingFile = (index: number) => {
    setPendingFiles((prev) => prev.filter((_, i) => i !== index));
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    const content = text.trim();
    if ((!content && pendingFiles.length === 0) || !canSend) return;

    setText("");
    const filesToUpload = [...pendingFiles];
    setPendingFiles([]);

    let attachments: Attachment[] | undefined;
    if (filesToUpload.length > 0) {
      setUploading(true);
      try {
        attachments = await Promise.all(
          filesToUpload.map((pf) => uploadFile(pf.file, pf.relativePath)),
        );
      } catch {
        setUploading(false);
        return;
      }
      setUploading(false);
    }

    await sendMessage(content || "See attached files.", attachments);
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
      {pendingFiles.length > 0 && (
        <div className="flex flex-wrap gap-1.5 mb-2 px-1">
          {pendingFiles.map((pf, i) => (
            <span
              key={i}
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
        </div>
      )}
      <div className="flex items-center gap-2 rounded-xl border border-border bg-surface-secondary px-3 py-2 focus-within:border-accent transition-colors">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFilesSelected}
        />
        <input
          ref={folderInputRef}
          type="file"
          // @ts-expect-error webkitdirectory is a non-standard attribute
          webkitdirectory=""
          className="hidden"
          onChange={handleFilesSelected}
        />
        <button
          type="button"
          onClick={() => fileInputRef.current?.click()}
          disabled={isDisabled}
          className="shrink-0 rounded-lg p-1 text-text-tertiary hover:text-text-secondary disabled:opacity-30 transition"
          title="Attach files"
        >
          <PaperClipIcon className="h-4 w-4" />
        </button>
        <button
          type="button"
          onClick={() => folderInputRef.current?.click()}
          disabled={isDisabled}
          className="shrink-0 rounded-lg p-1 text-text-tertiary hover:text-text-secondary disabled:opacity-30 transition"
          title="Attach folder"
        >
          <FolderIcon className="h-4 w-4" />
        </button>
        <textarea
          ref={textareaRef}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Send a message..."
          rows={1}
          className="flex-1 resize-none bg-transparent text-sm leading-5 text-text-primary placeholder:text-text-tertiary focus:outline-none m-0 p-0"
          disabled={isDisabled}
        />
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
            disabled={(!text.trim() && pendingFiles.length === 0) || !canSend || uploading}
            className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary disabled:opacity-30 transition"
          >
            <PaperAirplaneIcon className="h-5 w-5" />
          </button>
        )}
      </div>
    </form>
  );
}
