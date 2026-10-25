"use client";

import { useRef, useState, useEffect, useCallback } from "react";
import { useRouter } from "next/navigation";
import { ComposerPrimitive, ThreadPrimitive, AttachmentPrimitive, useAui, useAuiState, unstable_useTriggerPopoverScopeContextOptional } from "@assistant-ui/react";
import { useThreadIsRunning } from "@assistant-ui/core/react";
import { LexicalComposerInput } from "@assistant-ui/react-lexical";
import { useLexicalComposerContext } from "@lexical/react/LexicalComposerContext";
import { COMMAND_PRIORITY_NORMAL, KEY_ENTER_COMMAND } from "lexical";
import type { Unstable_TriggerItem } from "@assistant-ui/core";
import { PaperAirplaneIcon, StopIcon, PlusIcon, XMarkIcon } from "@heroicons/react/24/solid";
import { ArrowUpTrayIcon, CloudIcon, FolderOpenIcon } from "@heroicons/react/24/outline";
import { ComposerPastePlugin } from "./composer-paste-plugin";
import { FileBrowserModal } from "@/components/chat/file-browser-modal";
import { tryDispatchClientBuiltin } from "@/components/chat/client-commands";
import {
  useFronaTriggerAdapter,
  fronaSlashFormatter,
  fronaAtFormatter,
} from "@/components/chat/frona-trigger-adapter";
import { registerBackendAttachment, getBackendAttachments } from "@/lib/use-chat-runtime";
import { useContext } from "react";
import { usePendingTools } from "@/lib/pending-tools-context";
import { ChatContext } from "@/lib/chat-context";
import { useCommands } from "@/lib/use-commands";
import { api } from "@/lib/api-client";
import type { ToolWizardState } from "./external-tool-drawer";
import type { Attachment } from "@/lib/types";

function SubmitOnEnter() {
  const [editor] = useLexicalComposerContext();
  // The input's higher-priority handler lets slash/@ suggestions consume Enter first.
  useEffect(() => editor.registerCommand(KEY_ENTER_COMMAND, (event) => {
    if (!event || event.isComposing || event.shiftKey || event.ctrlKey || event.metaKey) return false;
    event.preventDefault();
    editor.getRootElement()?.closest("form")?.requestSubmit();
    return true;
  }, COMMAND_PRIORITY_NORMAL), [editor]);
  return null;
}

function ComposerAttachmentBadge() {
  const status = useAuiState((s) => s.attachment.status);
  return (
    <AttachmentPrimitive.Root className="inline-flex items-center gap-1 rounded-md bg-surface-tertiary px-2 py-1 text-xs text-text-secondary">
      <span className="max-w-[200px] truncate">
        <AttachmentPrimitive.Name />
      </span>
      {status.type === "running" && <span>Uploading…</span>}
      {status.type === "incomplete" && <span role="alert" className="text-error-text">{status.message}</span>}
      <AttachmentPrimitive.Remove asChild>
        <button type="button" aria-label="Remove attachment" className="ml-0.5 hover:text-text-primary">
          <XMarkIcon className="h-3 w-3" />
        </button>
      </AttachmentPrimitive.Remove>
    </AttachmentPrimitive.Root>
  );
}

/** `aui-trigger-popover` is the hook for the empty-popover hide rule in globals.css. */
const POPOVER_PANEL_CLASS =
  "aui-trigger-popover absolute bottom-full left-0 right-0 mb-2 max-h-64 overflow-y-auto rounded-xl border border-border bg-surface-secondary py-1 shadow-lg z-50";

function TriggerPopoverItemButton({ item, index }: { item: Unstable_TriggerItem; index: number }) {
  const ref = useRef<HTMLButtonElement>(null);
  const scope = unstable_useTriggerPopoverScopeContextOptional();
  const isHighlighted = scope?.highlightedIndex === index;

  // Popover marks `data-highlighted` but doesn't scroll into view itself.
  useEffect(() => {
    if (isHighlighted) {
      ref.current?.scrollIntoView({ block: "nearest" });
    }
  }, [isHighlighted]);

  return (
    <ComposerPrimitive.Unstable_TriggerPopoverItem
      ref={ref}
      item={item}
      index={index}
      className="flex w-full items-baseline gap-2 px-3 py-1.5 text-left text-sm text-text-secondary transition-colors hover:bg-surface-tertiary data-[highlighted]:bg-surface-tertiary data-[highlighted]:text-text-primary"
    >
      <span className="font-mono text-text-primary">{item.label}</span>
      {item.description && (
        <span className="ml-auto truncate text-xs text-text-tertiary">
          {item.description}
        </span>
      )}
    </ComposerPrimitive.Unstable_TriggerPopoverItem>
  );
}

export function FronaComposer({
  placeholder = "Send a message...",
  onSend,
  wizard,
}: {
  placeholder?: string;
  onSend?: (content: string, attachments?: Attachment[]) => void;
  wizard?: ToolWizardState;
}) {
  const composerRuntime = useAui().composer;
  const router = useRouter();
  const [menuOpen, setMenuOpen] = useState(false);
  const [browseOpen, setBrowseOpen] = useState(false);
  const [attachmentError, setAttachmentError] = useState<string | null>(null);
  const canSend = useAuiState((s) => s.composer.canSend);
  const attachmentsPending = useAuiState((s) => s.composer.attachments.some(
    (attachment) => attachment.status.type === "running" || attachment.status.type === "incomplete",
  ));
  const menuRef = useRef<HTMLDivElement>(null);
  const menuButtonRef = useRef<HTMLButtonElement>(null);

  const threadRunning = useThreadIsRunning();
  const pendingTools = usePendingTools();
  const chatCtx = useContext(ChatContext);
  const commands = useCommands(chatCtx?.chatId);

  const slashAdapter = useFronaTriggerAdapter(commands, "slash");
  const atAdapter = useFronaTriggerAdapter(commands, "at");

  // Client built-ins (e.g. /new) dispatch locally; clear the composer so the
  // chip isn't POSTed to the server.
  const handleDirectiveInserted = useCallback(
    (item: Unstable_TriggerItem) => {
      if (item.type !== "client-builtin") return;
      const name = (item.metadata?.["name"] as string | undefined) ?? item.id;
      void tryDispatchClientBuiltin(`/${name}`, {
        chatId: chatCtx?.chatId,
        agentId: chatCtx?.agentId,
        router,
      }).then(() => {
        composerRuntime.setText("");
      });
    },
    [composerRuntime, chatCtx?.chatId, chatCtx?.agentId, router],
  );

  const currentIndex = wizard?.currentIndex ?? 0;
  const safeIndex = Math.min(currentIndex, Math.max(0, pendingTools.length - 1));
  const currentPendingTool = pendingTools.length > 0 ? pendingTools[safeIndex] : undefined;

  const handleWizardAnswer = useCallback(() => {
    if (!currentPendingTool || !wizard) return;
    const text = composerRuntime.getState().text.trim();
    if (!text) return;
    composerRuntime.setText("");

    const kind = currentPendingTool.hitl?.request.type;
    const isChoiceKind = kind === "Question" || kind === "Takeover";
    let hitlResponse: import("@/lib/types").HitlResponse;
    if (isChoiceKind) {
      hitlResponse = { type: "Choice", data: text };
    } else if (kind === "App") {
      hitlResponse = { type: "Approval", data: false };
    } else if (kind === "Credential") {
      hitlResponse = { type: "Vault", data: { type: "Denied" } };
    } else {
      hitlResponse = { type: "Choice", data: text };
    }

    const nextAnswers = new Map(wizard.answers);
    nextAnswers.set(currentPendingTool.id, { hitlResponse, displayText: text });
    wizard.setAnswers(nextAnswers);

    const allNowAnswered = pendingTools.every((te) => nextAnswers.has(te.id));
    if (allNowAnswered && chatCtx?.chatId) {
      const resolutions = pendingTools.map((te) => {
        const ans = nextAnswers.get(te.id);
        return {
          tool_call_id: te.id,
          hitl_response: ans?.hitlResponse ?? ({ type: "Choice", data: "User declined to answer" } as const),
        };
      });
      wizard.setSubmitted(true);
      if (resolutions.length > 0) {
        api.post(`/api/chats/${chatCtx.chatId}/tool-calls/resolve`, { resolutions })
          .catch((err) => console.error("Failed to resolve tool calls", err));
      }
    } else if (safeIndex < pendingTools.length - 1) {
      wizard.setCurrentIndex(safeIndex + 1);
    }
  }, [currentPendingTool, wizard, composerRuntime, pendingTools, safeIndex, chatCtx?.chatId]);

  const activePlaceholder = currentPendingTool
    ? "Type your answer or click an option above..."
    : placeholder;

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

  const handleBrowseSelect = useCallback(
    (files: Attachment[]) => {
      for (const file of files) {
        registerBackendAttachment(file.path, file);
        composerRuntime.addAttachment({
          id: file.path,
          type: "file",
          name: file.filename,
          contentType: file.content_type,
          content: [{ type: "text", text: `[file: ${file.filename}]` }],
        });
      }
    },
    [composerRuntime],
  );

  return (
    <ComposerPrimitive.Unstable_TriggerPopoverRoot>
      <ComposerPrimitive.Unstable_TriggerPopover char="/" adapter={slashAdapter} className={POPOVER_PANEL_CLASS}>
        <ComposerPrimitive.Unstable_TriggerPopover.Directive
          formatter={fronaSlashFormatter}
          onInserted={handleDirectiveInserted}
        />
        <ComposerPrimitive.Unstable_TriggerPopoverItems>
          {(items) =>
            items.map((item, idx) => (
              <TriggerPopoverItemButton key={item.id} item={item} index={idx} />
            ))
          }
        </ComposerPrimitive.Unstable_TriggerPopoverItems>
      </ComposerPrimitive.Unstable_TriggerPopover>

      <ComposerPrimitive.Unstable_TriggerPopover char="@" adapter={atAdapter} className={POPOVER_PANEL_CLASS}>
        <ComposerPrimitive.Unstable_TriggerPopover.Directive
          formatter={fronaAtFormatter}
          onInserted={handleDirectiveInserted}
        />
        <ComposerPrimitive.Unstable_TriggerPopoverItems>
          {(items) =>
            items.map((item, idx) => (
              <TriggerPopoverItemButton key={item.id} item={item} index={idx} />
            ))
          }
        </ComposerPrimitive.Unstable_TriggerPopoverItems>
      </ComposerPrimitive.Unstable_TriggerPopover>

      <div className="relative">
        <ComposerPrimitive.Root
          className={currentPendingTool && !wizard?.submitted
            ? "bg-transparent p-4"
            : "rounded-2xl border border-border bg-surface-secondary p-4 focus-within:border-accent transition-colors"
          }
          onSubmit={(e: React.FormEvent) => {
            e.preventDefault();
            if (currentPendingTool) {
              handleWizardAnswer();
              return;
            }
            if (threadRunning || !composerRuntime.getState().canSend) return;
            const state = composerRuntime.getState();
            const text = state.text.trim();
            if (!text && !state.attachments.length) return;
            let backendAttachments: Attachment[];
            try {
              backendAttachments = getBackendAttachments(state.attachments);
            } catch (error) {
              setAttachmentError(error instanceof Error ? error.message : "Attachment unavailable");
              return;
            }
            setAttachmentError(null);
            if (onSend) {
              onSend(text, backendAttachments.length > 0 ? backendAttachments : undefined);
            } else {
              composerRuntime.send();
            }
          }}
        >
          {attachmentError && <p role="alert" className="mb-2 text-sm text-error-text">{attachmentError}</p>}
          <div className="flex flex-wrap gap-1.5 empty:hidden">
            <ComposerPrimitive.Attachments
              components={{ Attachment: ComposerAttachmentBadge }}
            />
          </div>
          <div>
            <LexicalComposerInput
              autoFocus
              placeholder={activePlaceholder}
              submitMode="none"
              className="aui-frona-composer-input w-full text-sm leading-5 text-text-primary"
            >
              <SubmitOnEnter />
              <ComposerPastePlugin onAttachmentError={setAttachmentError} />
            </LexicalComposerInput>
          </div>
          <div className="flex items-center justify-between pt-2">
            <div className="relative">
              <button
                ref={menuButtonRef}
                type="button"
                onClick={() => setMenuOpen((v) => !v)}
                className="rounded-lg p-1 text-text-tertiary hover:text-text-secondary hover:bg-surface-tertiary transition"
                title="Attach"
              >
                <PlusIcon className="h-5 w-5" />
              </button>
              {/* Keep AddAttachment mounted while the native picker is open;
                  unmounting it invalidates the pending file selection. */}
              <div
                ref={menuRef}
                hidden={!menuOpen}
                className="absolute bottom-full left-0 mb-1 w-max rounded-lg border border-border bg-surface-secondary py-1 shadow-lg"
              >
                <ComposerPrimitive.AddAttachment asChild multiple>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 px-3 py-2 text-sm text-text-secondary hover:bg-surface-tertiary transition-colors"
                    onClick={() => setMenuOpen(false)}
                  >
                    <ArrowUpTrayIcon className="h-4 w-4" />
                    Upload files
                  </button>
                </ComposerPrimitive.AddAttachment>
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
            </div>
            <div className="flex items-center gap-1">
              {wizard?.submitted && threadRunning ? (
                <ComposerPrimitive.Cancel asChild>
                  <button
                    type="button"
                    className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary transition"
                  >
                    <StopIcon className="h-5 w-5" />
                  </button>
                </ComposerPrimitive.Cancel>
              ) : (
              <>
              <ThreadPrimitive.If running>
                <ComposerPrimitive.Cancel asChild>
                  <button
                    type="button"
                    className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary transition"
                  >
                    <StopIcon className="h-5 w-5" />
                  </button>
                </ComposerPrimitive.Cancel>
              </ThreadPrimitive.If>
              <ThreadPrimitive.If running={false}>
                  <button
                    type="submit"
                    aria-label="Send message"
                    disabled={!currentPendingTool && (attachmentsPending || !canSend)}
                    className="shrink-0 rounded-lg p-1.5 text-text-secondary hover:text-text-primary transition"
                  >
                    <PaperAirplaneIcon className="h-5 w-5" />
                  </button>
              </ThreadPrimitive.If>
              </>
              )}
            </div>
          </div>
        </ComposerPrimitive.Root>
        <FileBrowserModal
          open={browseOpen}
          onClose={() => setBrowseOpen(false)}
          onSelect={handleBrowseSelect}
        />
      </div>
    </ComposerPrimitive.Unstable_TriggerPopoverRoot>
  );
}
