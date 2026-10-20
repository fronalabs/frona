import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { setAccessToken, uploadFile } from "../api-client";

const fetchMock = vi.fn();
beforeEach(() => {
  setAccessToken("test-token");
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => { setAccessToken(null); vi.unstubAllGlobals(); });

it.each([10 * 1024 * 1024 - 1, 10 * 1024 * 1024, 12 * 1024 * 1024])("uploads a %s-byte file using the server's configured limit", async (size) => {
  const backend = { path: "uploads/file", filename: "file.bin", content_type: "application/octet-stream", size_bytes: size, owner: "user" };
  fetchMock.mockResolvedValueOnce(new Response(JSON.stringify(backend), { status: 200 }));
  const file = new File([new Uint8Array(size)], "file.bin");
  await expect(uploadFile(file)).resolves.toEqual(backend);
  const [, options] = fetchMock.mock.calls[0];
  expect(options.body).toBeInstanceOf(FormData);
  expect(options.body.get("file").size).toBe(size);
  expect(options.headers).not.toHaveProperty("Content-Type");
});

it("shows the configured size limit returned by the server", async () => {
  fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({ error: "File too large (max 512 bytes)" }), { status: 413 }));
  await expect(uploadFile(new File([new Uint8Array(513)], "large.bin"))).rejects.toThrow("File too large (max 512 bytes)");
  expect(fetchMock).toHaveBeenCalledOnce();
});

it("explains a plain-text HTTP 413 response", async () => {
  fetchMock.mockResolvedValueOnce(new Response("body limit exceeded", { status: 413 }));
  await expect(uploadFile(new File(["x"], "file.txt"))).rejects.toThrow("Upload exceeds the server's size limit");
});

it("surfaces a network failure and allows a retry", async () => {
  fetchMock.mockRejectedValueOnce(new TypeError("Failed to fetch"));
  const file = new File(["x"], "file.txt");
  await expect(uploadFile(file)).rejects.toThrow("Server unavailable");
  const backend = { path: "uploads/retry", filename: "file.txt", content_type: "text/plain", size_bytes: 1, owner: "user" };
  fetchMock.mockResolvedValueOnce(new Response(JSON.stringify(backend), { status: 200 }));
  await expect(uploadFile(file)).resolves.toEqual(backend);
});
