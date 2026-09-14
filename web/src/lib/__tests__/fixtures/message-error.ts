import type { MessageError } from "../../types";

export function makeMessageError(message: string): MessageError {
  return {
    message,
    timestamp: "2026-09-14T10:10:09Z",
    details: {
      subsystem: "inference",
      data: { category: "invalid_request", retryable: false, provider: "openai", model: "gpt-5.3-codex", retry_count: 0, fallback_count: 0, http_status: 400 },
    },
  };
}
