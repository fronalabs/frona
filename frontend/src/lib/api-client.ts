const API_URL = process.env.NEXT_PUBLIC_API_URL || "http://localhost:3001";

class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

let accessToken: string | null = null;
let refreshPromise: Promise<string | null> | null = null;

export function setAccessToken(token: string | null) {
  accessToken = token;
}

export function getAccessToken(): string | null {
  return accessToken;
}

async function refreshAccessToken(): Promise<string | null> {
  try {
    const res = await fetch(`${API_URL}/api/auth/refresh`, {
      method: "POST",
      credentials: "include",
    });
    if (!res.ok) return null;
    const data = await res.json();
    accessToken = data.token;
    return accessToken;
  } catch {
    return null;
  }
}

async function ensureAccessToken(): Promise<string | null> {
  if (accessToken) return accessToken;
  if (refreshPromise) return refreshPromise;
  refreshPromise = refreshAccessToken().finally(() => {
    refreshPromise = null;
  });
  return refreshPromise;
}

async function request<T>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    ...((options.headers as Record<string, string>) || {}),
  };

  const token = await ensureAccessToken();
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }

  let res = await fetch(`${API_URL}${path}`, {
    ...options,
    headers,
    credentials: "include",
  });

  // On 401, try refreshing the access token and retry once
  if (res.status === 401 && token) {
    const newToken = await refreshAccessToken();
    if (newToken) {
      headers["Authorization"] = `Bearer ${newToken}`;
      res = await fetch(`${API_URL}${path}`, {
        ...options,
        headers,
        credentials: "include",
      });
    }
  }

  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new ApiError(res.status, body.error || "Request failed");
  }

  if (res.status === 204 || res.headers.get("content-length") === "0") {
    return undefined as T;
  }

  const text = await res.text();
  if (!text) {
    return undefined as T;
  }

  return JSON.parse(text);
}

import type { MessageResponse, Attachment } from "./types";

export async function uploadFile(file: File, relativePath?: string): Promise<Attachment> {
  const formData = new FormData();
  formData.append("file", file);
  if (relativePath) {
    formData.append("path", relativePath);
  }

  const token = await ensureAccessToken();
  const headers: Record<string, string> = {};
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }

  const res = await fetch(`${API_URL}/api/files`, {
    method: "POST",
    body: formData,
    headers,
  });

  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new ApiError(res.status, body.error || "Upload failed");
  }

  return res.json();
}

export async function presignFile(owner: string, path: string): Promise<string> {
  const data = await request<{ url: string }>("/api/files/presign", {
    method: "POST",
    body: JSON.stringify({ owner, path }),
  });
  return data.url;
}

export function fileDownloadUrl(attachment: Attachment, username: string): string {
  if (attachment.owner.startsWith("user:")) {
    return `${API_URL}/api/files/user/${username}/${attachment.path}`;
  } else if (attachment.owner.startsWith("agent:")) {
    const agentId = attachment.owner.replace("agent:", "");
    return `${API_URL}/api/files/agent/${agentId}/${attachment.path}`;
  }
  return "";
}

export async function streamMessage(
  chatId: string,
  body: { content: string; attachments?: Attachment[] },
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
  const token = await ensureAccessToken();
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }

  let res: Response;
  try {
    res = await fetch(`${API_URL}/api/chats/${chatId}/messages/stream`, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
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

export async function streamSession(
  callbacks: {
    onChatMessage: (chatId: string, message: MessageResponse) => void;
    onTaskUpdate: (taskId: string, status: string, sourceChatId: string | null, title: string, chatId: string | null, resultSummary: string | null) => void;
    onInferenceCount: (count: number) => void;
  },
  signal?: AbortSignal,
): Promise<void> {
  const token = await ensureAccessToken();
  const headers: Record<string, string> = {};
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }

  let res: Response;
  try {
    res = await fetch(`${API_URL}/api/stream`, { headers, signal });
  } catch (err) {
    if (err instanceof DOMException && err.name === "AbortError") return;
    return;
  }

  if (!res.ok) return;

  const reader = res.body?.getReader();
  if (!reader) return;

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
            switch (currentEvent) {
              case "chat_message":
                callbacks.onChatMessage(parsed.chat_id as string, parsed.message as MessageResponse);
                break;
              case "task_update":
                callbacks.onTaskUpdate(
                  parsed.task_id as string,
                  parsed.status as string,
                  parsed.source_chat_id as string | null,
                  parsed.title as string,
                  parsed.chat_id as string | null,
                  parsed.result_summary as string | null,
                );
                break;
              case "inference_count":
                callbacks.onInferenceCount(parsed.count as number);
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
    if (err instanceof DOMException && err.name === "AbortError") return;
  }
}

export async function cancelGeneration(chatId: string): Promise<void> {
  const token = await ensureAccessToken();
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (token) {
    headers["Authorization"] = `Bearer ${token}`;
  }
  await fetch(`${API_URL}/api/chats/${chatId}/cancel`, {
    method: "POST",
    headers,
  });
}

export async function cancelTask(taskId: string): Promise<void> {
  await request(`/api/tasks/${taskId}/cancel`, { method: "POST" });
}

export function deleteTask(taskId: string) {
  return request<void>(`/api/tasks/${taskId}`, { method: "DELETE" });
}

export function getTask(id: string) {
  return api.get<import("./types").TaskResponse>(`/api/tasks/${id}`);
}

export function archiveChat(chatId: string) {
  return request<import("./types").ChatResponse>(`/api/chats/${chatId}/archive`, { method: "POST" });
}

export function unarchiveChat(chatId: string) {
  return request<import("./types").ChatResponse>(`/api/chats/${chatId}/unarchive`, { method: "POST" });
}

export function deleteChat(chatId: string) {
  return request<void>(`/api/chats/${chatId}`, { method: "DELETE" });
}

export function getArchivedChats() {
  return request<import("./types").ChatResponse[]>("/api/chats/archived");
}

export const api = {
  get: <T>(path: string) => request<T>(path),
  post: <T>(path: string, body: unknown) =>
    request<T>(path, { method: "POST", body: JSON.stringify(body) }),
  put: <T>(path: string, body: unknown) =>
    request<T>(path, { method: "PUT", body: JSON.stringify(body) }),
  delete: <T>(path: string) => request<T>(path, { method: "DELETE" }),
};
