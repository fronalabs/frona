const API_URL = process.env.NEXT_PUBLIC_API_URL || "http://localhost:3001";

class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function request<T>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...((options.headers as Record<string, string>) || {}),
  };

  const res = await fetch(`${API_URL}${path}`, {
    ...options,
    headers,
    credentials: "include",
  });

  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new ApiError(res.status, body.error || "Request failed");
  }

  if (res.status === 204) {
    return undefined as T;
  }

  return res.json();
}

import type { MessageResponse } from "./types";

export async function streamMessage(
  chatId: string,
  body: { content: string },
  callbacks: {
    onUserMessage: (msg: MessageResponse) => void;
    onToken: (content: string) => void;
    onDone: (msg: MessageResponse) => void;
    onError: (error: Error) => void;
    onTitle?: (title: string) => void;
    onToolCall?: (name: string, args: unknown, description?: string) => void;
    onToolResult?: (name: string, result: string) => void;
    onEntityUpdated?: (table: string, recordId: string, fields: Record<string, unknown>) => void;
    onToolMessage?: (msg: MessageResponse) => void;
    onToolResolved?: (msg: MessageResponse) => void;
    onRateLimit?: (retryAfterSecs: number) => void;
    onCancelled?: () => void;
    onStreamEnd?: () => void;
  },
  signal?: AbortSignal,
): Promise<void> {
  let res: Response;
  try {
    res = await fetch(`${API_URL}/api/chats/${chatId}/messages/stream`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      credentials: "include",
      signal,
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === "AbortError") {
      return;
    }
    callbacks.onError(
      err instanceof Error ? err : new Error("Network error"),
    );
    return;
  }

  if (!res.ok) {
    const errorBody = await res.json().catch(() => ({ error: res.statusText }));
    callbacks.onError(new ApiError(res.status, errorBody.error || "Request failed"));
    return;
  }

  const reader = res.body?.getReader();
  if (!reader) {
    callbacks.onError(new Error("No response body"));
    return;
  }

  const decoder = new TextDecoder();
  let buffer = "";
  let currentEvent = "";
  let receivedDone = false;

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
          const data = line.slice(6);
          try {
            const parsed = JSON.parse(data);
            switch (currentEvent) {
              case "user_message":
                callbacks.onUserMessage(parsed as MessageResponse);
                break;
              case "token":
                callbacks.onToken(parsed.content as string);
                break;
              case "done":
                receivedDone = true;
                callbacks.onDone(parsed.message as MessageResponse);
                break;
              case "title":
                callbacks.onTitle?.(parsed.title as string);
                break;
              case "tool_call":
                callbacks.onToolCall?.(
                  parsed.name as string,
                  parsed.arguments,
                  parsed.description as string | undefined,
                );
                break;
              case "tool_result":
                callbacks.onToolResult?.(
                  parsed.name as string,
                  parsed.result as string,
                );
                break;
              case "entity_updated":
                callbacks.onEntityUpdated?.(
                  parsed.table as string,
                  parsed.record_id as string,
                  parsed.fields as Record<string, unknown>,
                );
                break;
              case "tool_message":
                callbacks.onToolMessage?.(parsed as MessageResponse);
                break;
              case "tool_resolved":
                callbacks.onToolResolved?.(parsed as MessageResponse);
                break;
              case "rate_limit":
                callbacks.onRateLimit?.(parsed.retry_after_secs as number);
                break;
              case "cancelled":
                callbacks.onCancelled?.();
                break;
            }
          } catch {
            // skip malformed JSON
          }
          currentEvent = "";
        }
      }
    }
  } catch (err) {
    if (err instanceof DOMException && err.name === "AbortError") {
      return;
    }
    callbacks.onError(
      err instanceof Error ? err : new Error("Stream read error"),
    );
  }

  if (!receivedDone) {
    callbacks.onStreamEnd?.();
  }
}

export async function cancelGeneration(chatId: string): Promise<void> {
  await fetch(`${API_URL}/api/chats/${chatId}/cancel`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
  });
}

export const api = {
  get: <T>(path: string) => request<T>(path),
  post: <T>(path: string, body: unknown) =>
    request<T>(path, { method: "POST", body: JSON.stringify(body) }),
  put: <T>(path: string, body: unknown) =>
    request<T>(path, { method: "PUT", body: JSON.stringify(body) }),
  delete: <T>(path: string) => request<T>(path, { method: "DELETE" }),
};
