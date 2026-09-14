import type { ErrorCategory, MessageError } from "./types";

export function messageProcessingError(error: unknown): MessageError {
  if (error instanceof Error && "messageError" in error && isMessageError(error.messageError)) {
    return error.messageError;
  }
  const status = error instanceof Error && "status" in error && typeof error.status === "number"
    ? error.status : undefined;
  const category: ErrorCategory = status === 0 ? "network"
    : status === 401 ? "authentication"
    : status === 403 ? "permission"
    : status === 408 || status === 504 ? "timeout"
    : status === 429 ? "rate_limit"
    : status && status >= 500 ? "internal"
    : status && status >= 400 ? "invalid_request"
    : "unknown";
  return {
    message: error instanceof Error ? error.message : "Message processing failed.",
    timestamp: new Date().toISOString(),
    details: {
      subsystem: "message_processing",
      data: { category, retryable: status === 0 || status === 429 || status === 502 || status === 503 || status === 504,
        ...(status ? { http_status: status } : {}) },
    },
  };
}

export function isMessageError(value: unknown): value is MessageError {
  if (!value || typeof value !== "object" || !("message" in value) || typeof value.message !== "string"
    || !("timestamp" in value) || typeof value.timestamp !== "string" || !("details" in value)) return false;
  const details = value.details;
  return !!details && typeof details === "object" && "subsystem" in details
    && ["inference", "tool_execution", "message_processing"].includes(String(details.subsystem))
    && "data" in details && !!details.data && typeof details.data === "object"
    && "category" in details.data && typeof details.data.category === "string"
    && "retryable" in details.data && typeof details.data.retryable === "boolean";
}
