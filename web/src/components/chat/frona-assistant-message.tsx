"use client";

import { useState, useEffect } from "react";
import { MessagePrimitive, useAuiState, useMessagePartText } from "@assistant-ui/react";
import { useThreadIsRunning } from "@assistant-ui/core/react";
import { MarkdownText } from "./markdown-text";
import { useSession } from "@/lib/session-context";
import { useNavigation } from "@/lib/navigation-context";
import { useRetryInfo } from "@/lib/retry-context";
import { formatFullTimestamp, formatTime, useTimezone } from "@/lib/format-time";
import { TimeMarkers } from "./time-markers";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "@/components/ui/code-block";
import { agentDisplayName } from "@/lib/types";
import type { Attachment } from "@/lib/types";
import { isMessageError } from "@/lib/message-error";
import { DefaultToolCallUI } from "./tool-uis/default-tool-call-ui";
import { ToolTimelineProvider } from "./tool-uis/tool-timeline-context";
import { ArrowDownTrayIcon, XMarkIcon, ClipboardDocumentListIcon, SparklesIcon, ExclamationCircleIcon } from "@heroicons/react/24/outline";
import { useRouter } from "next/navigation";
import * as Tooltip from "@radix-ui/react-tooltip";

function ReasoningToggle({ text, open, onToggle }: { text: string; open: boolean; onToggle: () => void }) {
  if (!text) return null;
  return (
    <Tooltip.Provider delayDuration={200}>
      <Tooltip.Root>
        <Tooltip.Trigger asChild>
          <button
            onClick={onToggle}
            aria-pressed={open}
            className={`inline-flex items-center transition cursor-pointer ${open ? "text-text-primary" : "text-text-tertiary hover:text-text-primary"}`}
          >
            <SparklesIcon className="h-3.5 w-3.5" />
          </button>
        </Tooltip.Trigger>
        <Tooltip.Portal>
          <Tooltip.Content
            side="top"
            sideOffset={4}
            className="z-50 rounded-lg bg-surface-secondary border border-border px-3 py-2 text-xs text-text-secondary shadow-lg animate-in fade-in-0 zoom-in-95"
          >
            {open ? "Hide reasoning" : "Show reasoning"}
            <Tooltip.Arrow className="fill-surface-secondary" />
          </Tooltip.Content>
        </Tooltip.Portal>
      </Tooltip.Root>
    </Tooltip.Provider>
  );
}

function ReasoningPanel({ text }: { text: string }) {
  return (
    <div className="mb-2 rounded-md border border-border bg-surface-secondary px-3 py-2 text-xs text-text-secondary whitespace-pre-wrap">
      {text}
    </div>
  );
}

function StreamingIndicator() {
  const message = useAuiState((state) => state.message);
  if (message.status?.type !== "running") return null;
  return (
    <span className="inline-flex items-center gap-1 py-1 -order-1">
      <span className="h-1 w-1 rounded-full bg-text-tertiary animate-[wave_1.4s_ease-in-out_infinite]" />
      <span className="h-1 w-1 rounded-full bg-text-tertiary animate-[wave_1.4s_ease-in-out_0.2s_infinite]" />
      <span className="h-1 w-1 rounded-full bg-text-tertiary animate-[wave_1.4s_ease-in-out_0.4s_infinite]" />
    </span>
  );
}

export function MessageError() {
  const status = useAuiState((state) => state.message.status);
  if (status?.type !== "incomplete" || status.reason !== "error") return null;
  const failure = isMessageError(status.error) ? status.error : undefined;
  const error = failure?.message
    ?? (typeof status.error === "string" && status.error.trim() ? status.error
    : "Message processing failed.");
  return (
    <div role="alert" className="mt-2 flex w-full items-start gap-2 rounded-lg border border-error-text/20 bg-error-bg px-3 py-2 text-sm text-error-text">
      <ExclamationCircleIcon className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
      <div className="min-w-0">
        <p className="font-medium">Unable to complete this reply</p>
        <p className="mt-1 whitespace-pre-wrap break-words [overflow-wrap:anywhere]">{error}</p>
        {failure && (
          <details className="mt-2">
            <summary className="cursor-pointer">Details</summary>
            <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
              <dt>Time</dt><dd><time dateTime={failure.timestamp}>{new Date(failure.timestamp).toLocaleString()}</time></dd>
              <dt>Subsystem</dt><dd>{({ inference: "Inference", tool_execution: "Tool execution", message_processing: "Message processing" })[failure.details.subsystem]}</dd>
              <dt>Category</dt><dd>{({ authentication: "Authentication", permission: "Permission", model_unavailable: "Model unavailable", rate_limit: "Rate limit", timeout: "Timeout", network: "Network", invalid_request: "Invalid request", invalid_response: "Invalid response", configuration: "Configuration", internal: "Internal", unknown: "Unknown" })[failure.details.data.category]}</dd>
              <dt>Retryable</dt><dd>{failure.details.data.retryable ? "Yes" : "No"}</dd>
              {failure.details.subsystem === "inference" && <>
                {failure.details.data.provider && <><dt>Provider</dt><dd>{failure.details.data.provider}</dd></>}
                {failure.details.data.model && <><dt>Model</dt><dd className="break-all">{failure.details.data.model}</dd></>}
                {failure.details.data.retry_count !== undefined && <><dt>Retries</dt><dd>{failure.details.data.retry_count}</dd></>}
                {failure.details.data.fallback_count !== undefined && <><dt>Fallbacks tried</dt><dd>{failure.details.data.fallback_count}</dd></>}
              </>}
              {failure.details.subsystem === "tool_execution" && failure.details.data.tool_name && <><dt>Tool</dt><dd>{failure.details.data.tool_name}</dd></>}
              {failure.details.data.http_status !== undefined && <><dt>HTTP status</dt><dd>{failure.details.data.http_status}</dd></>}
            </dl>
          </details>
        )}
      </div>
    </div>
  );
}

function AvatarContent({ avatar, letter }: { avatar?: string | null; letter: string }) {
  if (avatar) {
    return <img src={avatar} alt="" className="h-8 w-8 rounded-full object-cover" />;
  }
  return (
    <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-xs font-medium bg-surface-tertiary text-text-secondary">
      {letter}
    </div>
  );
}

function AgentAvatar({ name, avatar }: { name: string; avatar?: string | null }) {
  const letter = name.charAt(0).toUpperCase();

  return (
    <>
      <MessagePrimitive.If last={false}>
        <AvatarContent avatar={avatar} letter={letter} />
      </MessagePrimitive.If>
      <MessagePrimitive.If last>
        <LastMessageAvatar avatar={avatar} letter={letter} />
      </MessagePrimitive.If>
    </>
  );
}

function LastMessageAvatar({ avatar, letter }: { avatar?: string | null; letter: string }) {
  const isRunning = useThreadIsRunning();

  return (
    <div className="relative shrink-0 h-8 w-8">
      {isRunning && (
        <>
          <div className="absolute inset-[-3px] rounded-full animate-spin" style={{
            background: "conic-gradient(from 0deg, transparent 0%, var(--accent) 30%, transparent 60%)",
            mask: "radial-gradient(farthest-side, transparent calc(100% - 2px), #fff calc(100% - 2px))",
            WebkitMask: "radial-gradient(farthest-side, transparent calc(100% - 2px), #fff calc(100% - 2px))",
            animationDuration: "1.2s",
          }} />
          <div className="absolute inset-[-3px] rounded-full animate-spin" style={{
            background: "conic-gradient(from 180deg, transparent 0%, var(--accent) 20%, transparent 50%)",
            mask: "radial-gradient(farthest-side, transparent calc(100% - 2px), #fff calc(100% - 2px))",
            WebkitMask: "radial-gradient(farthest-side, transparent calc(100% - 2px), #fff calc(100% - 2px))",
            animationDuration: "1.2s",
            opacity: 0.5,
          }} />
        </>
      )}
      <AvatarContent avatar={avatar} letter={letter} />
    </div>
  );
}

const RETRY_LABELS: Record<string, string> = {
  rate_limited: "Rate limited",
  server_error: "Server error",
  network_error: "Network error",
  empty_response: "Empty response",
  timeout: "Timeout",
  overloaded: "Overloaded",
};

function RetryBadge() {
  const retry = useRetryInfo();
  const [remaining, setRemaining] = useState(0);

  useEffect(() => {
    if (!retry) return;
    const update = () => {
      const elapsed = (Date.now() - retry.startedAt) / 1000;
      const left = Math.max(0, Math.ceil(retry.retryAfterSecs - elapsed));
      setRemaining(left);
    };
    update();
    const id = setInterval(update, 1000);
    return () => clearInterval(id);
  }, [retry]);

  if (!retry) return null;

  const label = RETRY_LABELS[retry.reason] ?? retry.reason;

  return (
    <span className="inline-flex items-center gap-1.5 rounded-full bg-surface-tertiary px-2 py-0.5 text-[10px] font-medium text-text-secondary">
      <span className="h-2.5 w-2.5 animate-spin rounded-full border border-text-tertiary border-t-text-secondary" />
      {label} · retry {remaining}s
    </span>
  );
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function isPreviewable(contentType: string) {
  return contentType.startsWith("text/") || contentType === "application/json";
}

function FilePreviewModal({ attachment, onClose }: { attachment: Attachment; onClose: () => void }) {
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    if (!attachment.url) return;
    fetch(attachment.url)
      .then((r) => r.text())
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
            <a
              href={attachment.url}
              download={attachment.filename}
              className="flex items-center gap-1.5 rounded-md bg-surface-tertiary px-2.5 py-1.5 text-xs text-text-secondary hover:text-text-primary hover:bg-surface-secondary transition-colors"
            >
              <ArrowDownTrayIcon className="h-3.5 w-3.5" />
              Download
            </a>
            <button onClick={onClose} className="text-text-tertiary hover:text-text-primary transition-colors">
              <XMarkIcon className="h-5 w-5" />
            </button>
          </div>
        </div>
        <div className="flex-1 overflow-auto p-4">
          {loading ? (
            <p className="text-sm text-text-tertiary">Loading...</p>
          ) : attachment.content_type === "text/markdown" ? (
            <div className="prose prose-sm max-w-none text-text-primary prose-headings:text-text-primary prose-strong:text-text-primary prose-a:text-accent prose-code:text-text-primary prose-code:before:content-none prose-code:after:content-none prose-blockquote:text-text-secondary prose-blockquote:border-border">
              <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                components={{
                  code({ className, children, ...props }) {
                    const match = /language-(\w+)/.exec(className || "");
                    const code = String(children).replace(/\n$/, "");
                    if (match) return <CodeBlock code={code} language={match[1]} />;
                    return <code className={className} {...props}>{children}</code>;
                  },
                }}
              >
                {content ?? ""}
              </ReactMarkdown>
            </div>
          ) : (
            <pre className="whitespace-pre-wrap text-sm text-text-primary font-mono">{content}</pre>
          )}
        </div>
      </div>
    </div>
  );
}

function AttachmentItem({ attachment }: { attachment: Attachment }) {
  const url = attachment.url;
  const isImage = attachment.content_type.startsWith("image/");
  const canPreview = isPreviewable(attachment.content_type);
  const [showPreview, setShowPreview] = useState(false);

  if (!url) return null;

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
    <>
      <button
        onClick={canPreview ? () => setShowPreview(true) : undefined}
        className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-surface-tertiary px-3 py-2 text-xs text-text-secondary cursor-pointer hover:bg-surface-secondary transition-colors"
      >
        <span className="truncate max-w-[200px]">{attachment.filename}</span>
        {!canPreview && (
          <a href={url} download={attachment.filename} onClick={(e) => e.stopPropagation()}>
            <ArrowDownTrayIcon className="h-3.5 w-3.5 text-text-tertiary hover:text-text-primary" />
          </a>
        )}
      </button>
      {showPreview && <FilePreviewModal attachment={attachment} onClose={() => setShowPreview(false)} />}
    </>
  );
}

function MessageAttachments() {
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

export function FronaAssistantMessage() {
  const { agentId: sessionAgentId, activeTaskId } = useSession();
  const message = useAuiState((state) => state.message);
  const messageAgentId = (message.metadata as Record<string, any>)?.custom?.agentId;
  const agentId = messageAgentId ?? sessionAgentId ?? undefined;
  const { agents } = useNavigation();
  const tz = useTimezone();
  const router = useRouter();
  const [reasoningOpen, setReasoningOpen] = useState(false);

  const agent = agents.find((a) => a.id === agentId);
  const agentName = agentDisplayName(agentId, agent?.name);
  const custom = (message.metadata as Record<string, any>)?.custom ?? {};
  const isContinuation = custom.continuation;
  const reasoningText = (custom.reasoning as string | undefined) ?? "";
  const taskCompletion = custom.taskCompletion as
    | { task_id: string; status: string }
    | undefined;
  const showTaskBadge =
    taskCompletion && activeTaskId !== taskCompletion.task_id;
  const isoTime = message.createdAt?.toISOString();

  return (
    <MessagePrimitive.Root>
      <TimeMarkers daySeparator={custom.daySeparator} gap={custom.gap} />
      <div className="group w-full" data-message-id={message.id}>
        {!isContinuation && (
          <div className="flex items-center gap-2.5 h-8">
            <AgentAvatar name={agentName} avatar={agent?.avatar_url} />
            <p className="text-xs font-medium text-text-tertiary">
              {agentName}
            </p>
            {reasoningText && (
              <ReasoningToggle
                text={reasoningText}
                open={reasoningOpen}
                onToggle={() => setReasoningOpen((v) => !v)}
              />
            )}
            {showTaskBadge && (
              <Tooltip.Provider delayDuration={200}>
                <Tooltip.Root>
                  <Tooltip.Trigger asChild>
                    <button
                      onClick={() => router.push(`/chat?task=${taskCompletion!.task_id}`)}
                      className="inline-flex items-center text-text-tertiary hover:text-text-primary transition cursor-pointer"
                    >
                      <ClipboardDocumentListIcon className="h-3.5 w-3.5" />
                    </button>
                  </Tooltip.Trigger>
                  <Tooltip.Portal>
                    <Tooltip.Content
                      side="top"
                      sideOffset={4}
                      className="z-50 rounded-lg bg-surface-secondary border border-border px-3 py-2 text-xs text-text-secondary shadow-lg animate-in fade-in-0 zoom-in-95"
                    >
                      Open Task
                      <Tooltip.Arrow className="fill-surface-secondary" />
                    </Tooltip.Content>
                  </Tooltip.Portal>
                </Tooltip.Root>
              </Tooltip.Provider>
            )}
            {isoTime && (
              <time
                dateTime={isoTime}
                title={formatFullTimestamp(isoTime, tz)}
                className="text-xs text-text-tertiary opacity-0 group-hover:opacity-100 transition"
              >
                {formatTime(isoTime, tz)}
              </time>
            )}
            <MessagePrimitive.If last>
              <RetryBadge />
            </MessagePrimitive.If>
          </div>
        )}
        <div className={`${isContinuation ? "pl-[42px] -mt-1" : "pl-[42px]"} text-base text-text-primary flex flex-col items-start`}>
          {reasoningOpen && reasoningText && <ReasoningPanel text={reasoningText} />}
          <ToolTimelineProvider>
            <MessagePrimitive.Parts
              unstable_showEmptyOnNonTextEnd={false}
              components={{
                Text: SmoothMarkdownText,
                Empty: StreamingIndicator,
                tools: {
                  Fallback: DefaultToolCallUI,
                },
              }}
            />
          </ToolTimelineProvider>
          <MessageError />
        </div>
      </div>
    </MessagePrimitive.Root>
  );
}

function SmoothMarkdownText() {
  const { text } = useMessagePartText();
  const isRunning = useThreadIsRunning();

  if (!text && isRunning) return <StreamingIndicator />;
  if (!text) return null;
  return <span className="w-full"><MarkdownText smooth /></span>;
}
