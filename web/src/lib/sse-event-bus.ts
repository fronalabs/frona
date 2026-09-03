import { ensureAccessToken, API_URL } from "./api-client";
import type { MessageResponse, Notification, PauseReason } from "./types";


export type ChatSSEEvent =
  | { type: "token"; content: string }
  | { type: "reasoning"; content: string }
  | { type: "tool_call"; id: string; provider_call_id: string; name: string; arguments: unknown; description?: string }
  | { type: "tool_result"; name: string; success: boolean; summary?: string }
  | { type: "chat_message"; message: MessageResponse }
  | { type: "retry"; retryAfterSecs: number; reason: string }
  | { type: "inference_start" }
  | { type: "inference_done"; message: MessageResponse }
  | { type: "inference_cancelled"; reason: string }
  | { type: "inference_error"; error: string }
  | { type: "inference_paused"; reason: PauseReason; message: MessageResponse }
  | { type: "inference_resume"; message: MessageResponse }
  | { type: "usage_recorded"; usage: UsageRecorded };

/// One row of inference accounting — fired per provider call, gated on
/// chat_id. See `crates/frona-server/src/inference/metrics/service.rs`.
export interface UsageRecorded {
  chat_id: string;
  user_id: string | null;
  agent_id: string | null;
  message_id: string | null;
  kind_tag: string;
  model_ref: string;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  cost_usd: number | null;
  duration_ms: number;
  fallback_index: number;
}


export type GlobalSSEEvent =
  | { type: "title"; chatId: string; title: string }
  | { type: "entity_updated"; chatId: string; table: string; recordId: string; fields: Record<string, unknown> }
  | { type: "task_update"; taskId: string; status: string; sourceChatId: string | null; title: string; chatId: string | null; resultSummary: string | null }
  | { type: "activity_changed" }
  | { type: "inference_count"; count: number }
  | { type: "notification"; notification: Notification };


interface ChatSubscriber {
  chatId: string;
  push: (event: ChatSSEEvent) => void;
  close: () => void;
  /** Soft-reset on SSE reconnect: clear buffered events, resolve any active read with done. */
  notifyReconnect: () => void;
}

type GlobalListener = (event: GlobalSSEEvent) => void;

type ChatEventListener = (chatId: string, event: ChatSSEEvent) => void;

type ReconnectListener = () => void;

export class SSEEventBus {
  private chatSubscribers = new Map<string, Set<ChatSubscriber>>();
  private chatBuffers = new Map<string, ChatSSEEvent[]>();
  private chatEventListeners = new Set<ChatEventListener>();
  private globalListeners = new Set<GlobalListener>();
  private reconnectListeners = new Set<ReconnectListener>();
  private activeSignal: AbortSignal | null = null;

  connect(signal: AbortSignal): void {
    if (this.activeSignal && !this.activeSignal.aborted) return;
    this.activeSignal = signal;
    this.runLoop(signal);
  }

  /** Register a callback that fires when the SSE stream reconnects after a drop. */
  onReconnect(callback: ReconnectListener): () => void {
    this.reconnectListeners.add(callback);
    return () => this.reconnectListeners.delete(callback);
  }

  subscribe(chatId: string, signal: AbortSignal): AsyncIterable<ChatSSEEvent> {
    const bus = this;
    return {
      [Symbol.asyncIterator]() {
        const buffered = bus.chatBuffers.get(chatId);
        const queue: ChatSSEEvent[] = buffered ? buffered.splice(0) : [];
        if (buffered && buffered.length === 0) bus.chatBuffers.delete(chatId);

        let resolve: ((value: IteratorResult<ChatSSEEvent>) => void) | null = null;
        let done = false;

        const subscriber: ChatSubscriber = {
          chatId,
          push(event) {
            if (done) return;
            if (resolve) {
              const r = resolve;
              resolve = null;
              r({ value: event, done: false });
            } else {
              queue.push(event);
            }
          },
          close() {
            done = true;
            if (resolve) {
              const r = resolve;
              resolve = null;
              r({ value: undefined as unknown as ChatSSEEvent, done: true });
            }
          },
          notifyReconnect() {
            queue.length = 0;
            if (resolve) {
              const r = resolve;
              resolve = null;
              r({ value: undefined as unknown as ChatSSEEvent, done: true });
            }
          },
        };

        let subs = bus.chatSubscribers.get(chatId);
        if (!subs) {
          subs = new Set();
          bus.chatSubscribers.set(chatId, subs);
        }
        subs.add(subscriber);

        const cleanup = () => {
          subscriber.close();
          subs!.delete(subscriber);
          if (subs!.size === 0) bus.chatSubscribers.delete(chatId);
        };

        signal.addEventListener("abort", cleanup);

        return {
          next(): Promise<IteratorResult<ChatSSEEvent>> {
            if (queue.length > 0) {
              return Promise.resolve({ value: queue.shift()!, done: false });
            }
            if (done) {
              return Promise.resolve({ value: undefined as unknown as ChatSSEEvent, done: true });
            }
            return new Promise((r) => { resolve = r; });
          },
          return(): Promise<IteratorResult<ChatSSEEvent>> {
            cleanup();
            return Promise.resolve({ value: undefined as unknown as ChatSSEEvent, done: true });
          },
        };
      },
    };
  }

  onGlobal(callback: GlobalListener): () => void {
    this.globalListeners.add(callback);
    return () => this.globalListeners.delete(callback);
  }

  /** Observe chat events without consuming the chat-specific stream. */
  onChatEvent(callback: ChatEventListener): () => void {
    this.chatEventListeners.add(callback);
    return () => this.chatEventListeners.delete(callback);
  }

  private dispatchChat(chatId: string, event: ChatSSEEvent) {
    console.log("[sse-bus] dispatchChat", event.type, chatId);
    for (const listener of this.chatEventListeners) {
      try {
        listener(chatId, event);
      } catch {
        // An observer must not interrupt delivery to the owning chat.
      }
    }
    const subs = this.chatSubscribers.get(chatId);
    if (subs) {
      for (const sub of subs) {
        sub.push(event);
      }
      return;
    }
    let buffer = this.chatBuffers.get(chatId);
    if (!buffer) {
      buffer = [];
      this.chatBuffers.set(chatId, buffer);
    }
    buffer.push(event);
  }

  private dispatchGlobal(event: GlobalSSEEvent) {
    for (const listener of this.globalListeners) {
      listener(event);
    }
  }

  private notifySubscribersReconnect() {
    for (const subs of this.chatSubscribers.values()) {
      for (const sub of subs) {
        sub.notifyReconnect();
      }
    }
    // Preserve buffers for chats with no subscriber yet (React effect race).
    for (const chatId of [...this.chatBuffers.keys()]) {
      if (this.chatSubscribers.has(chatId)) {
        this.chatBuffers.delete(chatId);
      }
    }
  }

  private async runLoop(signal: AbortSignal) {
    let delay = 1000;
    const maxDelay = 30000;
    let hadConnection = false;

    while (!signal.aborted) {
      try {
        const isReconnect = hadConnection;
        await this.connectStream(signal, isReconnect ? () => {
          this.notifySubscribersReconnect();
          for (const listener of this.reconnectListeners) {
            try { listener(); } catch { /* ignore */ }
          }
        } : undefined);
        hadConnection = true;
        delay = 1000;
      } catch {
        if (signal.aborted) return;
      }
      if (signal.aborted) return;
      await new Promise((r) => setTimeout(r, delay));
      delay = Math.min(delay * 2, maxDelay);
    }
    if (this.activeSignal === signal) {
      this.activeSignal = null;
    }
  }

  private async connectStream(signal: AbortSignal, onConnected?: () => void): Promise<void> {
    const tokenResult = await ensureAccessToken();
    const headers: Record<string, string> = {};
    if (tokenResult.ok) headers["Authorization"] = `Bearer ${tokenResult.token}`;

    let res: Response;
    try {
      res = await fetch(`${API_URL}/api/stream`, { headers, signal, credentials: "include" });
    } catch (err) {
      if (err instanceof DOMException && err.name === "AbortError") return;
      throw err;
    }

    if (!res.ok) throw new Error(`Stream connection failed: ${res.status}`);

    const reader = res.body?.getReader();
    if (!reader) return;

    onConnected?.();

    const decoder = new TextDecoder();
    let buffer = "";
    let currentEvent = "";

    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";

        for (const line of lines) {
          if (line.startsWith("event: ")) {
            currentEvent = line.slice(7).trim();
          } else if (line.startsWith("data: ")) {
            try {
              const parsed = JSON.parse(line.slice(6));
              const chatId = (parsed.chat_id as string) ?? "";
              this.routeEvent(currentEvent, chatId, parsed);
            } catch {
            }
            currentEvent = "";
          }
        }
      }
    } catch (err) {
      if (err instanceof DOMException && err.name === "AbortError") return;
      throw err;
    }
  }

  routeEvent(eventType: string, chatId: string, parsed: Record<string, unknown>) {
    switch (eventType) {
      case "token":
        this.dispatchChat(chatId, { type: "token", content: parsed.content as string });
        break;
      case "reasoning":
        this.dispatchChat(chatId, { type: "reasoning", content: parsed.content as string });
        break;
      case "tool_call":
        this.dispatchChat(chatId, {
          type: "tool_call",
          id: parsed.id as string,
          provider_call_id: parsed.provider_call_id as string,
          name: parsed.name as string,
          arguments: parsed.arguments,
          description: parsed.description as string | undefined,
        });
        break;
      case "tool_result":
        this.dispatchChat(chatId, {
          type: "tool_result",
          name: parsed.name as string,
          success: parsed.success as boolean,
          summary: parsed.summary as string | undefined,
        });
        break;
      case "chat_message":
        this.dispatchChat(chatId, { type: "chat_message", message: parsed.message as MessageResponse });
        break;
      case "retry":
        this.dispatchChat(chatId, {
          type: "retry",
          retryAfterSecs: parsed.retry_after_secs as number,
          reason: parsed.reason as string,
        });
        break;
      case "inference_start":
        this.dispatchChat(chatId, { type: "inference_start" });
        break;
      case "inference_done":
        this.dispatchChat(chatId, { type: "inference_done", message: parsed.message as MessageResponse });
        break;
      case "inference_cancelled":
        this.dispatchChat(chatId, { type: "inference_cancelled", reason: parsed.reason as string });
        break;
      case "inference_error":
        this.dispatchChat(chatId, { type: "inference_error", error: parsed.error as string });
        break;
      case "inference_paused":
        this.dispatchChat(chatId, {
          type: "inference_paused",
          reason: parsed.reason as PauseReason,
          message: parsed.message as MessageResponse,
        });
        break;
      case "inference_resume":
        this.dispatchChat(chatId, { type: "inference_resume", message: parsed.message as MessageResponse });
        break;
      case "usage_recorded":
        this.dispatchChat(chatId, {
          type: "usage_recorded",
          usage: parsed as unknown as UsageRecorded,
        });
        break;
      case "title":
        this.dispatchGlobal({ type: "title", chatId, title: parsed.title as string });
        break;
      case "entity_updated":
        this.dispatchGlobal({
          type: "entity_updated",
          chatId,
          table: parsed.table as string,
          recordId: parsed.record_id as string,
          fields: parsed.fields as Record<string, unknown>,
        });
        break;
      case "task_update":
        this.dispatchGlobal({
          type: "task_update",
          taskId: parsed.task_id as string,
          status: parsed.status as string,
          sourceChatId: parsed.source_chat_id as string | null,
          title: parsed.title as string,
          chatId: parsed.chat_id as string | null,
          resultSummary: parsed.result_summary as string | null,
        });
        break;
      case "inference_count":
        this.dispatchGlobal({ type: "inference_count", count: parsed.count as number });
        break;
      case "activity_changed":
        this.dispatchGlobal({ type: "activity_changed" });
        break;
      case "notification":
        this.dispatchGlobal({ type: "notification", notification: parsed.notification as Notification });
        break;
    }
  }
}

export const sseBus = new SSEEventBus();
