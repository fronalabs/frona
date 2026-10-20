"use client";

import { useMemo, useEffect, useCallback, useRef, useSyncExternalStore } from "react";
import { MessageNotSentError, useExternalStoreRuntime } from "@assistant-ui/react";
import type { CompleteAttachment, AppendMessage, AttachmentAdapter, PendingAttachment } from "@assistant-ui/react";
import type { ExternalStoreAdapter } from "@assistant-ui/react";
import { ChatStore, type RetryInfo } from "./chat-store";
import { sseBus } from "./sse-event-bus";
import { sendMessage as apiSendMessage, cancelGeneration, api, uploadFile } from "./api-client";
import { computeTimeMarkers, useTimezone } from "./format-time";
import type { MessageResponse, ChatResponse, Attachment } from "./types";
import { renderMessageBody } from "./task-result-render";
import { messageProcessingError } from "./message-error";


const backendAttachmentRegistry = new Map<string, Attachment>();

export function registerBackendAttachment(id: string, attachment: Attachment) {
  backendAttachmentRegistry.set(id, attachment);
}

export function getBackendAttachment(id: string): Attachment | undefined {
  return backendAttachmentRegistry.get(id);
}

export function getBackendAttachments(attachments: readonly (PendingAttachment | CompleteAttachment)[]): Attachment[] {
  return attachments.map((attachment) => {
    if (attachment.status.type === "running") {
      throw new Error(`Wait for "${attachment.name}" to finish uploading.`);
    }
    if (attachment.status.type === "incomplete") {
      throw new Error(attachment.status.message ?? `Remove and reattach "${attachment.name}" before sending.`);
    }
    const backend = getBackendAttachment(attachment.id);
    if (!backend) {
      throw new Error(`"${attachment.name}" is no longer available. Remove and reattach it before sending.`);
    }
    return backend;
  });
}

function completeAttachment(
  attachment: Pick<CompleteAttachment, "id" | "name" | "contentType" | "file">,
  url?: string,
): CompleteAttachment {
  const imageUrl = attachment.contentType?.startsWith("image/") ? url?.trim() : undefined;
  return {
    ...attachment,
    type: imageUrl ? "image" : "file",
    status: { type: "complete" },
    content: imageUrl
      ? [{ type: "image", image: imageUrl }]
      : [{ type: "text", text: `[file: ${attachment.name}]` }],
  };
}

function convertBackendAttachment(att: Attachment, file?: File): CompleteAttachment {
  registerBackendAttachment(att.path, att);
  return completeAttachment({ id: att.path, name: att.filename, contentType: att.content_type, ...(file ? { file } : {}) }, att.url);
}

export const fronaAttachmentAdapter: AttachmentAdapter = {
  accept: "*",

  async *add({ file }: { file: File }): AsyncGenerator<PendingAttachment> {
    const attachment: PendingAttachment = {
      id: crypto.randomUUID(),
      type: "file",
      name: file.name,
      contentType: file.type,
      status: { type: "running", reason: "uploading", progress: 0 },
      content: [],
      file,
    };
    yield attachment;
    try {
      const uploaded = await uploadFile(file);
      backendAttachmentRegistry.set(attachment.id, uploaded);
      yield {
        ...attachment,
        name: uploaded.filename,
        contentType: uploaded.content_type,
        status: { type: "requires-action", reason: "composer-send" },
      };
    } catch (error) {
      const detail = error instanceof Error ? error.message : "Upload failed";
      yield {
        ...attachment,
        status: {
          type: "incomplete", reason: "error",
          message: `Could not upload "${file.name}": ${detail}. Remove and reattach it to retry.`,
        },
      };
    }
  },

  async send(attachment: PendingAttachment): Promise<CompleteAttachment> {
    return completeAttachment(attachment, getBackendAttachment(attachment.id)?.url);
  },

  async remove(attachment) {
    backendAttachmentRegistry.delete(attachment.id);
  },
};


export type AssistantContentPart =
  | { type: "text"; text: string }
  | { type: "reasoning"; text: string }
  | { type: "tool-call"; toolCallId: string; toolName: string; args: Record<string, string | number | boolean | null>; argsText: string; result?: string };

/**
 * Put every tool turn's text before the final assistant text, with each item
 * on its own Markdown line, then strip it from the tool args to avoid rendering
 * it twice when the tool timeline is expanded.
 */
export function appendTurnText(parts: AssistantContentPart[]): AssistantContentPart[] {
  const textPart = parts.find((p) => p.type === "text");
  const turnTexts = parts.flatMap((p) => {
    if (p.type !== "tool-call") return [];
    const turnText = (p.args as Record<string, unknown>)?.turnText;
    return typeof turnText === "string" && turnText.trim() ? [turnText.trim()] : [];
  });
  if (turnTexts.length === 0) return parts;

  const existingText = textPart?.type === "text" ? textPart.text.trimEnd() : "";
  // Blank lines ensure Markdown renders every turn on a distinct visible line.
  const combinedText = [...turnTexts, existingText].filter(Boolean).join("\n\n");

  const promoted = parts.map(p => {
    if (p.type === "text") return { ...p, text: combinedText };
    if (p.type === "tool-call" && (p.args as Record<string, unknown>)?.turnText) {
      const { turnText: _, ...rest } = p.args;
      return { ...p, args: rest };
    }
    return p;
  });

  // An internal-tool turn with reasoning can have no text placeholder.
  if (!textPart) {
    return [{ type: "text", text: combinedText }, ...promoted];
  }
  return promoted;
}

function stripTurnText(parts: AssistantContentPart[]): AssistantContentPart[] {
  return parts.map((part) => {
    if (part.type !== "tool-call" || !(part.args as Record<string, unknown>)?.turnText) {
      return part;
    }
    const { turnText: _, ...args } = part.args;
    return { ...part, args };
  });
}

export function convertMessage(msg: MessageResponse, localImageFiles?: ReadonlyMap<string, File>) {
  if (msg.role === "user" || msg.role === "contact" || msg.role === "livecall") {
    const attachments = msg.attachments?.map((att) => convertBackendAttachment(att, localImageFiles?.get(att.path)));
    return {
      id: msg.id,
      role: "user" as const,
      content: [{ type: "text" as const, text: msg.content || "" }],
      createdAt: new Date(msg.created_at),
      ...(attachments?.length ? { attachments } : {}),
      metadata: {
        custom: {
          originalRole: msg.role,
          contactId: msg.contact_id,
          daySeparator: msg._daySeparator,
          gap: msg._gap,
          command: msg.command,
        },
      },
    };
  }

  if (msg.role === "agent" || msg.role === "taskcompletion" || (msg.role === "system" && msg.event)) {
    // TaskCompletion with nothing to show (complex schema with no recognized
    // text field, no attachments) → suppress the bubble entirely. Failed
    // tasks still surface so the user can see the failure.
    if (
      msg.role === "taskcompletion" &&
      msg.event?.type === "TaskCompletion" &&
      msg.event.data.status !== "Failed" &&
      !msg.attachments?.length &&
      !renderMessageBody(msg).trim()
    ) {
      return null;
    }

    const content: AssistantContentPart[] = [];

    if (msg.tool_calls?.length) {
      for (const te of msg.tool_calls) {
        if (te.hitl) {
          const toolName = te.hitl.request.type;
          const status = te.hitl.status;
          const resolved = status === "resolved" || status === "denied";
          const args: Record<string, string | number | boolean | null> = {
            prompt: te.hitl.prompt,
            url: te.hitl.url,
            status,
          };
          for (const [k, v] of Object.entries(te.hitl.request.data)) {
            if (v == null || typeof v === "string" || typeof v === "number" || typeof v === "boolean") {
              args[k] = v as string | number | boolean | null;
            } else {
              args[k] = JSON.stringify(v);
            }
          }
          const responseText =
            te.hitl.response == null
              ? null
              : te.hitl.response.type === "Approval"
                ? te.hitl.response.data
                  ? "Approved"
                  : "Denied"
                : te.hitl.response.type === "Choice"
                  ? te.hitl.response.data
                  : te.hitl.response.data.type === "Granted"
                    ? "Granted"
                    : "Denied";
          content.push({
            type: "tool-call",
            toolCallId: te.id,
            toolName,
            args,
            argsText: JSON.stringify(args),
            ...(resolved && responseText != null ? { result: responseText } : {}),
            ...(resolved && responseText == null ? { result: String(status) } : {}),
          });
        }
      }
    }

    const bodyText = renderMessageBody(msg);
    if (bodyText) {
      content.push({ type: "text", text: bodyText });
    }
    if (!bodyText && !msg.reasoning && !msg.event) {
      content.push({ type: "text", text: "" });
    }

    if (msg.attachments?.length) {
      content.push({
        type: "tool-call",
        toolCallId: `__attachments_${msg.id}`,
        toolName: "Attachments",
        args: {} as Record<string, string | number | boolean | null>,
        argsText: "{}",
        result: "done",
      });
    }

    if (msg.tool_calls?.length) {
      for (const te of msg.tool_calls) {
        if (te.task_event) {
          const toolName = te.task_event.type;
          const data = te.task_event.data as Record<string, unknown>;
          const args: Record<string, string | number | boolean | null> = {};
          for (const [k, v] of Object.entries(data)) {
            if (v == null || typeof v === "string" || typeof v === "number" || typeof v === "boolean") {
              args[k] = v as string | number | boolean | null;
            } else {
              args[k] = JSON.stringify(v);
            }
          }
          content.push({
            type: "tool-call",
            toolCallId: te.id,
            toolName,
            args,
            argsText: JSON.stringify(args),
            result: String(data.status ?? "done"),
          });
        }
      }
    }

    if (msg.tool_calls?.length) {
      for (const te of msg.tool_calls) {
        if (!te.hitl && !te.task_event) {
          content.push({
            type: "tool-call",
            toolCallId: te.id,
            toolName: te.name,
            args: {
              description: te.description ?? te.name,
              ...te.arguments,
              ...(te.turn_text ? { turnText: te.turn_text } : {}),
              ...(!te.success ? { isError: true } : {}),
            } as Record<string, string | number | boolean | null>,
            argsText: JSON.stringify(te.arguments || {}),
            result: te.result,
          });
        }
      }
    }

    // Streaming text already contains the text emitted before each tool call,
    // so keep it in the main message and remove duplicate timeline copies.
    // Completed backend messages put persisted turn_text before the final text.
    const inFlight = msg.status === "executing" || msg.status === "paused";
    const finalContent = inFlight ? stripTurnText(content) : appendTurnText(content);

    return {
      id: msg.id,
      role: "assistant" as const,
      content: finalContent,
      createdAt: new Date(msg.created_at),
      status: msg.status === "failed"
        ? {
            type: "incomplete" as const,
            reason: "error" as const,
            error: msg.error ?? "Message processing failed.",
          }
        : msg.tool_calls?.some(te => te.hitl?.status === "pending")
        ? { type: "requires-action" as const, reason: "tool-calls" as const }
        : inFlight
          ? { type: "running" as const }
          : { type: "complete" as const, reason: "stop" as const },
      metadata: {
        custom: {
          agentId: msg.agent_id,
          originalRole: msg.role,
          continuation: msg._continuation,
          daySeparator: msg._daySeparator,
          gap: msg._gap,
          ...(msg.reasoning ? { reasoning: msg.reasoning } : {}),
          ...(msg.attachments?.length ? { attachments: msg.attachments } : {}),
          ...(msg.role === "taskcompletion" && msg.event?.type === "TaskCompletion"
            ? {
                taskCompletion: {
                  task_id: msg.event.data.task_id,
                  status: msg.event.data.status,
                },
              }
            : {}),
        },
      },
    };
  }

  return null;
}


export interface ChatRuntimeOptions {
  chatId?: string;
  agentId: string;
  onChatCreated?: (chat: ChatResponse) => void;
}

export function useChatRuntime({ chatId, agentId, onChatCreated }: ChatRuntimeOptions) {
  // Only URL-less image previews need the local File; keep them within this runtime's lifetime.
  const localImageFiles = useRef(new Map<string, File>()).current;
  const currentChatIdRef = useRef<string | null>(chatId ?? null);
  currentChatIdRef.current = chatId ?? currentChatIdRef.current;
  const onChatCreatedRef = useRef(onChatCreated);
  onChatCreatedRef.current = onChatCreated;

  // One store per ChatView mount - persists across chatId changes (pending → real)
  const storeRef = useRef<ChatStore | null>(null);
  if (!storeRef.current) {
    storeRef.current = new ChatStore();
  }
  const store = storeRef.current;

  // Eager SSE subscription controller - set in onNew so events are captured
  // immediately, before the useEffect has a chance to fire.
  const eagerSubRef = useRef<AbortController | null>(null);

  const subscribe = useCallback((cb: () => void) => store.subscribe(cb), [store]);
  const storeSnapshot = useSyncExternalStore(
    subscribe,
    () => store.getSnapshot(),
  );

  // Load messages for existing chats. Skip if the store already has messages
  // (new chat: optimistic user message was added before chatId existed).
  useEffect(() => {
    if (chatId && store.messages.length === 0) {
      store.loadMessages(chatId);
    } else {
      store.markLoaded();
    }
  }, [chatId, store]);

  // Subscribe to SSE events for the chat.
  // If an eager subscription was started in onNew, adopt it instead of creating a new one.
  useEffect(() => {
    if (!chatId) return;

    if (eagerSubRef.current) {
      const adopted = eagerSubRef.current;
      eagerSubRef.current = null;
      return () => adopted.abort();
    }

    const controller = new AbortController();
    const events = sseBus.subscribe(chatId, controller.signal);

    (async () => {
      for await (const event of events) {
        store.handleEvent(event);
      }
    })();

    return () => controller.abort();
  }, [store, chatId]);

  useEffect(() => {
    return sseBus.onReconnect(() => {
      const id = currentChatIdRef.current;
      if (id) store.loadMessages(id);
    });
  }, [store]);

  const onNew = useCallback(async (message: AppendMessage) => {
    const text = message.content
      .filter((p): p is { type: "text"; text: string } => p.type === "text")
      .map((p) => p.text)
      .join("");

    let attachments: Attachment[];
    try {
      attachments = getBackendAttachments(message.attachments ?? []);
    } catch (error) {
      store.failMessage(messageProcessingError(error));
      // This control error restores the installed runtime's unsent composer draft.
      throw new MessageNotSentError(error instanceof Error ? error.message : "Attachment unavailable");
    }

    let sendChatId = currentChatIdRef.current;
    if (!sendChatId && !onChatCreatedRef.current) return;
    message.attachments?.forEach((attachment, index) => {
      const backend = attachments[index];
      if (!backend.url?.trim() && backend.content_type.startsWith("image/") && attachment.file?.type.startsWith("image/")) {
        localImageFiles.set(backend.path, attachment.file);
      }
    });
    store.addUserMessage(text, attachments.length ? attachments : undefined);

    try {
      if (!sendChatId) {
        // Standalone composers (home/space page) handle chat creation themselves.
        // Only create a chat here if there's a promotion callback.
        const chat = await api.post<ChatResponse>("/api/chats", { agent_id: agentId });
        sendChatId = chat.id;
        currentChatIdRef.current = sendChatId;

        // Eagerly subscribe to SSE events NOW, before the React effect fires.
        // This eliminates the race window where events could arrive unbuffered.
        const eager = new AbortController();
        eagerSubRef.current = eager;
        const events = sseBus.subscribe(sendChatId, eager.signal);
        (async () => {
          for await (const event of events) {
            store.handleEvent(event);
          }
        })();

        // This triggers slot promotion -> chatId prop change -> SSE effect adopts the eager sub.
        onChatCreatedRef.current!(chat);
      }

      const body = attachments.length
        ? { content: text, attachments }
        : { content: text };
      await apiSendMessage(sendChatId, body);
    } catch (error) {
      store.failMessage(messageProcessingError(error));
    }
  }, [agentId, store, localImageFiles]);

  const onCancel = useCallback(async () => {
    const id = currentChatIdRef.current;
    if (id) {
      await cancelGeneration(id).catch(() => {});
    }
  }, []);

  const timeZone = useTimezone();

  // Filter out messages that convertMessage returns null for (e.g. signal-only
  // task completions) and annotate each with day-boundary / large-gap markers.
  // Markers compute on the post-filter list so a hidden message can't create
  // a phantom separator.
  const filteredMessages = useMemo(() => {
    const kept = storeSnapshot.messages.filter((msg) => convertMessage(msg) !== null);
    const markers = computeTimeMarkers(kept, timeZone);
    // Annotate each assistant message with the prior message's command (if any),
    // so the bubble renders as a compact command-response when applicable.
    return kept.map((msg) => {
      const marker = markers.get(msg.id);
      if (!marker) return msg;
      return { ...msg, _daySeparator: marker.daySeparator, _gap: marker.gap };
    });
  }, [storeSnapshot.messages, timeZone]);

  const adapter: ExternalStoreAdapter<MessageResponse> = useMemo(() => ({
    messages: filteredMessages,
    isRunning: storeSnapshot.isRunning,
    convertMessage: (msg: MessageResponse) => convertMessage(msg, localImageFiles) ?? {
      id: msg.id,
      role: "assistant" as const,
      content: [],
      createdAt: new Date(msg.created_at),
      status: { type: "complete" as const, reason: "stop" as const },
    },
    onNew,
    onCancel,
    onAddToolResult: ({ toolCallId, result }) => {
      store.resolveToolCall(toolCallId, String(result ?? ""));
    },
    adapters: {
      attachments: fronaAttachmentAdapter,
    },
  }), [filteredMessages, storeSnapshot.isRunning, onNew, onCancel, store, localImageFiles]);

  const runtime = useExternalStoreRuntime(adapter);

  const sendMessage = useCallback((content: string, attachments?: Attachment[]) => {
    if (attachments?.length) {
      for (const att of attachments) {
        registerBackendAttachment(att.path, att);
      }
    }
    runtime.thread.append({
      role: "user",
      content: [{ type: "text", text: content }],
      attachments: attachments?.map((attachment) => convertBackendAttachment(attachment)),
    });
  }, [runtime]);

  const loadOlder = useCallback(() => {
    const id = currentChatIdRef.current;
    if (id) store.loadOlder(id);
  }, [store]);

  return {
    runtime,
    loaded: storeSnapshot.loaded,
    sendMessage,
    retryInfo: storeSnapshot.retryInfo,
    pendingTools: storeSnapshot.pendingTools,
    hasMore: storeSnapshot.hasMore,
    loadingMore: storeSnapshot.loadingMore,
    loadOlder,
    usagePerChat: storeSnapshot.usagePerChat,
    lastFallbackIndex: storeSnapshot.lastFallbackIndex,
    lastChatInputTokens: storeSnapshot.lastChatInputTokens,
    totalToolCalls: storeSnapshot.totalToolCalls,
  };
}

export type { RetryInfo };
