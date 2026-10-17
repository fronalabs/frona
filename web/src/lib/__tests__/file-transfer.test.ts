import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { downloadFile, fetchFile, fileRequestUrl, fileResponseFilename } from "../file-transfer";
import { setAccessToken } from "../api-client";

const fetchMock = vi.fn();
beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  fetchMock.mockReset();
  setAccessToken("private-session-token");
});
afterEach(() => {
  setAccessToken(null);
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("file requests", () => {
  it.each(["/api/files/user/alice/dashboard.html", "/api/files/agent/bot/chart.svg", "/s/private-share"])(
    "authenticates private downloads at %s and asks for attachment mode", async (path) => {
      fetchMock.mockResolvedValue(new Response("file"));
      await fetchFile(path, true);
      expect(fetchMock).toHaveBeenCalledWith(`${path}?download=true`, expect.objectContaining({
        headers: { Authorization: "Bearer private-session-token" }, credentials: "include",
      }));
    },
  );
  it("preserves signatures and queries without attaching session credentials", async () => {
    fetchMock.mockResolvedValue(new Response("signed"));
    await fetchFile("/api/files/user/alice/report.html?presign=a%2Bb%2Fc&other=hello&download=false", true);
    const [request, options] = fetchMock.mock.calls[0];
    const url = new URL(request);
    expect(url.searchParams.get("presign")).toBe("a+b/c");
    expect(url.searchParams.get("other")).toBe("hello");
    expect(url.searchParams.getAll("download")).toEqual(["true"]);
    expect(options).toEqual({ credentials: "omit" });
  });
  it("leaves preview presentation inline", async () => {
    fetchMock.mockResolvedValue(new Response("preview"));
    await fetchFile("/api/files/user/alice/dashboard.html");
    expect(fetchMock.mock.calls[0][0]).toBe("/api/files/user/alice/dashboard.html");
  });
  it.each([
    "https://untrusted.example/api/files/user/alice/report.html?signature=x",
    "https://localhost.evil.example/api/files/user/alice/report.html",
    "/api/auth/refresh",
    "/api/files/user/alice/../../../auth/refresh",
    "blob:http://localhost/opaque-id",
  ])("does not send credentials or alter unrelated URL %s", async (input) => {
    fetchMock.mockResolvedValue(new Response("external"));
    await fetchFile(input, true);
    expect(fetchMock).toHaveBeenCalledWith(new URL(input, window.location.origin).href, { credentials: "omit" });
  });
  it.each(["javascript:alert(1)", "data:text/html,test", "https://user:pass@example.com/file"])(
    "rejects unsafe URL %s", (input) => {
      expect(() => fileRequestUrl(input, true)).toThrow("Unsupported file URL");
    },
  );
  it("refreshes an expired session before retrying the attachment request", async () => {
    fetchMock.mockResolvedValueOnce(new Response("expired", { status: 401 }))
      .mockResolvedValueOnce(Response.json({ token: "fresh-token" }))
      .mockResolvedValueOnce(new Response("file"));
    await fetchFile("/s/private-share", true);
    expect(fetchMock.mock.calls[2]).toEqual(["/s/private-share?download=true", expect.objectContaining({
      headers: { Authorization: "Bearer fresh-token" },
    })]);
  });
});

describe("saving downloaded bytes", () => {
  it("uses the filename and releases the temporary anchor and blob", async () => {
    vi.useFakeTimers();
    const create = vi.fn().mockReturnValue("blob:http://localhost/download");
    const revoke = vi.fn();
    vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: create, revokeObjectURL: revoke }));
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      expect(this.download).toBe("dashboard.html");
      expect(this.href).toBe("blob:http://localhost/download");
      expect(this.isConnected).toBe(true);
    });
    fetchMock.mockResolvedValue(new Response("<script>untrusted()</script>"));
    await downloadFile("/api/files/user/alice/dashboard.html", "dashboard.html");
    expect(click).toHaveBeenCalledOnce();
    expect(document.querySelector("a[download]")).toBeNull();
    expect(revoke).not.toHaveBeenCalled();
    vi.runAllTimers();
    expect(revoke).toHaveBeenCalledWith("blob:http://localhost/download");
  });
  it("surfaces authorization failures instead of downloading an error page", async () => {
    fetchMock.mockResolvedValue(new Response("Forbidden", { status: 403 }));
    await expect(downloadFile("/api/files/user/alice/private.html", "private.html"))
      .rejects.toThrow("Download failed (403)");
  });
});


describe("file response filenames", () => {
  it("prefers the Unicode filename over the ASCII fallback", () => {
    const response = new Response("", { headers: {
      "Content-Disposition": "inline; filename=\"report.html\"; filename*=UTF-8''r%C3%A9sum%C3%A9.html",
    } });
    expect(fileResponseFilename(response, "/s/id")).toBe("résumé.html");
  });
  it("falls back safely when the extended filename is malformed", () => {
    const response = new Response("", { headers: {
      "Content-Disposition": "inline; filename=\"report.html\"; filename*=UTF-8''%ZZ",
    } });
    expect(fileResponseFilename(response, "/s/id")).toBe("report.html");
    expect(fileResponseFilename(new Response(""), "/api/files/user/u/a%20b.html?presign=secret")).toBe("a b.html");
  });
});

describe("a separately hosted backend", () => {
  it("authenticates only configured backend file URLs, including a deployment prefix", async () => {
    vi.stubEnv("NEXT_PUBLIC_FRONA_SERVER_BACKEND_URL", "https://api.example/frona");
    vi.resetModules();
    try {
      const client = await import("../api-client");
      const transfer = await import("../file-transfer");
      client.setAccessToken("backend-token");
      fetchMock.mockResolvedValue(new Response("file"));
      await transfer.fetchFile("/api/files/user/alice/dashboard.html", true);
      expect(fetchMock).toHaveBeenLastCalledWith("https://api.example/frona/api/files/user/alice/dashboard.html?download=true",
        expect.objectContaining({ headers: { Authorization: "Bearer backend-token" } }));
      await transfer.fetchFile("https://api.example/frona/s/shared", true);
      expect(fetchMock).toHaveBeenLastCalledWith("https://api.example/frona/s/shared?download=true",
        expect.objectContaining({ headers: { Authorization: "Bearer backend-token" } }));
      await transfer.fetchFile("https://api.example/other/api/files/user/alice/dashboard.html", true);
      expect(fetchMock).toHaveBeenLastCalledWith("https://api.example/other/api/files/user/alice/dashboard.html", { credentials: "omit" });
      client.setAccessToken(null);
    } finally {
      vi.unstubAllEnvs();
      vi.resetModules();
    }
  });
});
