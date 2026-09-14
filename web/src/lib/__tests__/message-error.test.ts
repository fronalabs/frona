import { afterEach, expect, it, vi } from "vitest";
import { api, ApiError, setAccessToken } from "../api-client";
import { messageProcessingError } from "../message-error";
import { makeMessageError } from "./fixtures/message-error";

afterEach(() => { vi.unstubAllGlobals(); setAccessToken(null); });

it("preserves server details and the original timestamp on failed API requests", async () => {
  const failure = makeMessageError("Inference service error");
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({
    error: failure.message, message_error: failure,
  }), { status: 502, headers: { "Content-Type": "application/json" } })));
  setAccessToken("test-token");
  const error = await api.get("/api/chats/test/messages").catch(error => error);
  expect(error).toBeInstanceOf(ApiError);
  expect(messageProcessingError(error)).toEqual(failure);
});

it("classifies HTTP failures without server details and records the time", () => {
  const before = Date.now();
  const failure = messageProcessingError(new ApiError(429, "Rate limited"));
  expect(failure.details).toEqual({ subsystem: "message_processing", data: { category: "rate_limit", retryable: true, http_status: 429 } });
  expect(Date.parse(failure.timestamp)).toBeGreaterThanOrEqual(before);
  expect(Date.parse(failure.timestamp)).toBeLessThanOrEqual(Date.now());
});

it("keeps network failures distinct from HTTP failures", () => {
  expect(messageProcessingError(new ApiError(0, "Server unavailable", "unavailable")).details)
    .toEqual({ subsystem: "message_processing", data: { category: "network", retryable: true } });
});
