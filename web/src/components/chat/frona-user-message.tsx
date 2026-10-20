"use client";

import { useCallback, useEffect, useState } from "react";
import { MessagePrimitive, AttachmentPrimitive, useAuiState } from "@assistant-ui/react";
import type { CompleteAttachment } from "@assistant-ui/react";
import { presignFile } from "@/lib/api-client";
import { getBackendAttachment } from "@/lib/use-chat-runtime";
import { formatFullTimestamp, formatTime, useTimezone } from "@/lib/format-time";
import { useNavigation } from "@/lib/navigation-context";
import type { MessageCommand } from "@/lib/types";
import { MarkdownText } from "./markdown-text";
import { TimeMarkers } from "./time-markers";

function InvocationChip({ command, prefix }: { command: MessageCommand; prefix: string }) {
  const { agents } = useNavigation();
  const tail = (command.type === "skill" ? command.prompt : command.args).trim();

  const matchedAgent =
    command.type === "command"
      ? agents.find((a) => a.handle === command.name)
      : undefined;
  const displayName = matchedAgent?.name ?? command.name;

  const directiveType =
    command.type === "skill"
      ? "skill"
      : matchedAgent || prefix === "@"
        ? "agent"
        : "command";
  return (
    <div className="text-base text-text-primary">
      <span className="aui-directive-chip" data-directive-type={directiveType}>
        {prefix}{displayName}
      </span>
      {tail && <span>{tail}</span>}
    </div>
  );
}

function CommandTypeBadge({ command }: { command: MessageCommand }) {
  const { agents } = useNavigation();
  const isAgent =
    command.type === "command" && agents.some((a) => a.handle === command.name);
  if (isAgent) return null;
  const label = command.type === "skill" ? "Skill" : "Command";
  return (
    <span className="inline-flex items-center rounded-full bg-surface-tertiary px-1.5 py-px text-[9px] font-medium uppercase tracking-wide text-text-tertiary">
      {label}
    </span>
  );
}

function useAttachmentUrl(attachment: CompleteAttachment, isImage: boolean) {
  const backend = getBackendAttachment(attachment.id);
  const owner = backend?.owner;
  const path = backend?.path;
  const signedUrl = attachment.content.find((part) => part.type === "image")?.image.trim()
    || backend?.url?.trim() || null;
  const file = !signedUrl && isImage && attachment.file?.type.startsWith("image/") ? attachment.file : undefined;
  const source = `${owner ?? ""}\0${attachment.id}\0${signedUrl ?? ""}`;
  const [resolved, setResolved] = useState<{ source: string; file?: File; url: string } | null>(null);
  const [refreshSource, setRefreshSource] = useState<string | null>(null);
  const refresh = refreshSource === source;

  useEffect(() => {
    let cancelled = false;
    const blobUrl = file ? URL.createObjectURL(file) : undefined;
    if (blobUrl) {
      setResolved({ source, file, url: blobUrl });
    }
    if (owner && path && (!isImage || !signedUrl || refresh)) {
      presignFile(owner, path).then((url) => {
        if (!cancelled && url.trim()) {
          setResolved({ source, file, url: url.trim() });
        }
      }).catch(() => {});
    }
    return () => {
      cancelled = true;
      if (blobUrl) URL.revokeObjectURL(blobUrl);
    };
  }, [source, signedUrl, file, refresh, owner, path, isImage]);

  return {
    url: resolved?.source === source && resolved.file === file ? resolved.url : signedUrl,
    refresh: () => { if (signedUrl) setRefreshSource(source); },
  };
}

function UserAttachment({ attachment }: { attachment: CompleteAttachment }) {
  const isImage = attachment.type === "image" || !!attachment.contentType?.startsWith("image/");
  const { url, refresh } = useAttachmentUrl(attachment, isImage);

  const handleClick = useCallback(() => {
    if (url) window.open(url, "_blank");
  }, [url]);

  if (isImage && url) {
    return (
      <AttachmentPrimitive.Root
        className="overflow-hidden rounded-lg border border-border cursor-pointer hover:opacity-90 transition-opacity"
        onClick={handleClick}
      >
        <img
          src={url}
          onError={refresh}
          alt={attachment.name}
          className="max-h-72 max-w-full object-contain"
        />
      </AttachmentPrimitive.Root>
    );
  }

  if (isImage) {
    return (
      <AttachmentPrimitive.Root className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-surface-tertiary px-3 py-2 text-xs text-text-secondary">
        <AttachmentPrimitive.Name />
      </AttachmentPrimitive.Root>
    );
  }

  return (
    <AttachmentPrimitive.Root
      className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-surface-tertiary px-3 py-2 text-xs text-text-secondary cursor-pointer hover:bg-surface-secondary transition-colors"
      onClick={handleClick}
    >
      <AttachmentPrimitive.Name />
    </AttachmentPrimitive.Root>
  );
}

export function FronaUserMessage() {
  const message = useAuiState((state) => state.message);
  const tz = useTimezone();
  const custom = (message.metadata as Record<string, any>)?.custom ?? {};
  const command: MessageCommand | undefined = custom.command;
  const rawText =
    (message.content[0] as { type?: string; text?: string } | undefined)?.type === "text"
      ? ((message.content[0] as { text?: string }).text ?? "")
      : "";
  const prefix = rawText.charAt(0) === "@" ? "@" : "/";
  const isoTime = message.createdAt?.toISOString();
  return (
    <MessagePrimitive.Root>
      <TimeMarkers daySeparator={custom.daySeparator} gap={custom.gap} />
      <div className="group w-full" data-message-id={message.id}>
        <div className="flex items-center gap-2.5 h-8">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-accent text-surface">
            Y
          </div>
          <p className="text-xs font-medium text-text-tertiary">You</p>
          {command && <CommandTypeBadge command={command} />}
          {isoTime && (
            <time
              dateTime={isoTime}
              title={formatFullTimestamp(isoTime, tz)}
              className="text-xs text-text-tertiary opacity-0 group-hover:opacity-100 transition"
            >
              {formatTime(isoTime, tz)}
            </time>
          )}
        </div>
        <div className="pl-[42px] text-base text-text-primary">
          {command ? (
            <InvocationChip command={command} prefix={prefix} />
          ) : (
            <MessagePrimitive.Parts
              components={{
                Text: MarkdownText,
              }}
            />
          )}
          <div className="flex flex-wrap gap-2 mt-2 empty:hidden">
            <MessagePrimitive.Attachments>
              {({ attachment }) => <UserAttachment attachment={attachment} />}
            </MessagePrimitive.Attachments>
          </div>
        </div>
      </div>
    </MessagePrimitive.Root>
  );
}
