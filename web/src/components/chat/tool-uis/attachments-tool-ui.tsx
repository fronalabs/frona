"use client";

import { useEffect, useState } from "react";
import { makeAssistantToolUI, useAuiState } from "@assistant-ui/react";
import { ArrowDownTrayIcon, XMarkIcon } from "@heroicons/react/24/outline";
import { FilePreviewContent, canPreviewFile, languageFromFilename } from "@/components/preview/file-preview-content";
import { FileDownloadButton } from "@/components/preview/file-download-button";
import { fetchFile } from "@/lib/file-transfer";
import type { Attachment } from "@/lib/types";

function FilePreviewModal({ attachment, onClose }: { attachment: Attachment; onClose: () => void }) {
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const isMarkdown = attachment.content_type === "text/markdown" || attachment.filename.endsWith(".md") || attachment.filename.endsWith(".mdx");
  const isCode = !isMarkdown && !!languageFromFilename(attachment.filename);

  useEffect(() => {
    if (!attachment.url) return;
    fetchFile(attachment.url)
      .then((r) => { if (!r.ok) throw new Error("Failed to load file"); return r.text(); })
      .then((text) => { setContent(text); setLoading(false); })
      .catch(() => { setContent("Failed to load file."); setLoading(false); });
  }, [attachment.url]);

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50" onClick={onClose}>
      <div
        className="relative flex flex-col w-[90vw] max-w-3xl max-h-[80vh] rounded-xl border border-border bg-surface shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-4 py-3 border-b border-border">
          <span className="text-sm font-medium text-text-primary truncate">{attachment.filename}</span>
          <div className="flex items-center gap-2">
            <FileDownloadButton
              url={attachment.url!}
              filename={attachment.filename}
              className="flex items-center gap-1.5 rounded-md bg-surface-tertiary px-2.5 py-1.5 text-xs text-text-secondary hover:text-text-primary hover:bg-surface-secondary transition-colors"
            >
              <ArrowDownTrayIcon className="h-3.5 w-3.5" />
              Download
            </FileDownloadButton>
            <button onClick={onClose} className="text-text-tertiary hover:text-text-primary transition-colors">
              <XMarkIcon className="h-5 w-5" />
            </button>
          </div>
        </div>
        <div className={`flex-1 overflow-auto ${isCode ? "" : "p-4"}`}>
          {loading ? (
            <p className="text-sm text-text-tertiary p-4">Loading...</p>
          ) : (
            <FilePreviewContent
              content={content ?? ""}
              filename={attachment.filename}
              contentType={attachment.content_type}
            />
          )}
        </div>
      </div>
    </div>
  );
}

function AttachmentItem({ attachment }: { attachment: Attachment }) {
  const url = attachment.url;
  const isImage = attachment.content_type.startsWith("image/");
  const canPreview = canPreviewFile(attachment.content_type, attachment.filename);
  const [showPreview, setShowPreview] = useState(false);

  if (!url) return null;

  if (isImage) {
    return (
      <a href={url} target="_blank" rel="noopener noreferrer">
        <img src={url} alt={attachment.filename} className="max-w-xs max-h-48 rounded-md border border-border" />
      </a>
    );
  }

  if (canPreview) {
    return (
      <>
        <button
          onClick={() => setShowPreview(true)}
          className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-surface-tertiary px-3 py-2 text-xs text-text-secondary cursor-pointer hover:bg-surface-secondary transition-colors"
        >
          <span className="truncate max-w-[200px]">{attachment.filename}</span>
        </button>
        {showPreview && <FilePreviewModal attachment={attachment} onClose={() => setShowPreview(false)} />}
      </>
    );
  }

  return (
    <FileDownloadButton
      url={url}
      filename={attachment.filename}
      className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-surface-tertiary px-3 py-2 text-xs text-text-secondary cursor-pointer hover:bg-surface-secondary transition-colors"
    >
      <span className="truncate max-w-[200px]">{attachment.filename}</span>
      <ArrowDownTrayIcon className="h-3.5 w-3.5 text-text-tertiary" />
    </FileDownloadButton>
  );
}

function AttachmentsRender() {
  const message = useAuiState((state) => state.message);
  const attachments = (message.metadata as Record<string, any>)?.custom?.attachments as Attachment[] | undefined;

  if (!attachments?.length) return null;

  return (
    <div className="flex flex-wrap gap-2 mt-2">
      {attachments.map((att, i) => (
        <AttachmentItem key={i} attachment={att} />
      ))}
    </div>
  );
}

export const AttachmentsToolUI = makeAssistantToolUI<Record<string, never>, string>({
  toolName: "Attachments",
  render: AttachmentsRender,
});
