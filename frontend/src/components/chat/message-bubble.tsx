"use client";

import type { MessageResponse, Attachment } from "@/lib/types";
import { useNavigation } from "@/lib/navigation-context";
import { fileDownloadUrl } from "@/lib/api-client";
import { MarkdownContent } from "./markdown-content";

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function AttachmentItem({ attachment }: { attachment: Attachment }) {
  const url = fileDownloadUrl(attachment.path);
  const isImage = attachment.content_type.startsWith("image/");

  if (isImage) {
    return (
      <a href={url} target="_blank" rel="noopener noreferrer">
        <img
          src={url}
          alt={attachment.filename}
          className="max-w-xs max-h-48 rounded-md border border-border"
        />
      </a>
    );
  }

  return (
    <a
      href={url}
      target="_blank"
      rel="noopener noreferrer"
      className="inline-flex items-center gap-1.5 rounded-md bg-surface-tertiary px-2.5 py-1.5 text-xs text-text-secondary hover:text-text-primary transition"
    >
      <span className="truncate max-w-[200px]">{attachment.filename}</span>
      <span className="text-text-tertiary">({formatFileSize(attachment.size_bytes)})</span>
    </a>
  );
}

interface MessageBubbleProps {
  message: MessageResponse;
  agentName: string;
}

export function MessageBubble({ message, agentName }: MessageBubbleProps) {
  const isUser = message.role === "user";
  const { agents } = useNavigation();

  const displayName = isUser
    ? "You"
    : message.agent_id
      ? (agents.find((a) => a.id === message.agent_id)?.name ?? agentName)
      : agentName;

  const attachments = message.attachments ?? [];

  return (
    <div className="flex justify-start">
      <div className="flex items-start gap-2.5 max-w-[85%]">
        <div
          className={`flex h-7 w-7 shrink-0 items-center justify-center rounded-full text-xs font-medium ${
            isUser
              ? "bg-accent text-surface"
              : "bg-surface-tertiary text-text-secondary"
          }`}
        >
          {isUser ? "U" : displayName.charAt(0).toUpperCase()}
        </div>
        <div className="min-w-0 pt-0.5">
          <p className="text-[11px] font-medium text-text-tertiary mb-0.5">
            {displayName}
          </p>
          <div className="text-sm text-text-primary">
            <MarkdownContent content={message.content} />
          </div>
          {attachments.length > 0 && (
            <div className="flex flex-wrap gap-2 mt-2">
              {attachments.map((att, i) => (
                <AttachmentItem key={i} attachment={att} />
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
