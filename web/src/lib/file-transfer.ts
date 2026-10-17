import { API_URL, apiFetch } from "./api-client";

/** Only file endpoints on the configured backend may receive session credentials. */
export function fileRequestUrl(input: string, download = false): { url: URL; apiPath: string | null } {
  const backend = new URL(API_URL || window.location.origin, window.location.origin);
  const basePath = backend.pathname.replace(/\/$/, "");
  const isApiRelative = /^\/(?:api\/files\/(?:user|agent)\/|s\/)/.test(input);
  const url = new URL(isApiRelative ? `${API_URL}${input}` : input, window.location.origin);
  if (!["http:", "https:", "blob:"].includes(url.protocol) || url.username || url.password) {
    throw new Error("Unsupported file URL");
  }
  const path = url.pathname.slice(basePath.length);
  const trusted = url.origin === backend.origin && url.pathname.startsWith(`${basePath}/`) &&
    /^\/(?:api\/files\/(?:user|agent)\/[^/]+\/.+|s\/[^/]+)$/.test(path);
  if (download && trusted) url.searchParams.set("download", "true");
  return { url, apiPath: trusted ? `${path}${url.search}` : null };
}

export async function fetchFile(input: string, download = false): Promise<Response> {
  const { url, apiPath } = fileRequestUrl(input, download);
  // A presign already supplies authorization. Avoid requiring a session for
  // shared files, or adding the user's Bearer token to a signed URL.
  return apiPath && !url.searchParams.has("presign")
    ? apiFetch(apiPath)
    : fetch(url.href, { credentials: "omit" });
}

export async function downloadFile(input: string, filename: string): Promise<void> {
  const response = await fetchFile(input, true);
  if (!response.ok) throw new Error(`Download failed (${response.status})`);
  const objectUrl = URL.createObjectURL(await response.blob());
  const anchor = document.createElement("a");
  anchor.href = objectUrl;
  anchor.download = filename;
  anchor.hidden = true;
  document.body.appendChild(anchor);
  try {
    anchor.click();
  } finally {
    anchor.remove();
    // Give the browser time to consume the URL before releasing the blob.
    window.setTimeout(() => URL.revokeObjectURL(objectUrl), 1000);
  }
}

/** Decode the server's RFC 8187 filename, then fall back to the final URL. */
export function fileResponseFilename(response: Response, fallback: string): string {
  const disposition = response.headers.get("Content-Disposition");
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(disposition ?? "");
  let filename: string | undefined;
  if (encoded) {
    try { filename = decodeURIComponent(encoded[1]); } catch { /* Use the plain fallback. */ }
  }
  filename ??= /filename="([^"\r\n]*)"/i.exec(disposition ?? "")?.[1];
  if (!filename) {
    const segment = new URL(response.url || fallback, window.location.origin).pathname.split("/").pop();
    try { filename = decodeURIComponent(segment ?? ""); } catch { filename = segment; }
  }
  // A response filename is display data, never a directory or control sequence.
  return filename?.split(/[\\/]/).pop()?.replace(/[\x00-\x1f\x7f]/g, "") || "download";
}
