"use client";

import { useCallback, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowDownTrayIcon } from "@heroicons/react/24/outline";
import { fileResponseFilename } from "@/lib/file-transfer";
import { apiFetch, getAccessToken } from "@/lib/api-client";
import { FileDownloadButton } from "@/components/preview/file-download-button";
import { FilePreviewContent } from "@/components/preview/file-preview-content";

/** Short form (`/s/{id}`) honours the `Share::File.public` flag — public
 *  shares mint a presigned URL so logged-out viewers can see the file.
 *  Long form (`/api/files/...`) has no share row and always requires
 *  `FileAuth`. */
export type PreviewSource =
  | { kind: "short"; id: string }
  | { kind: "long"; owner: string; handle: string; path: string };

type FetchState =
  | { status: "loading" }
  | { status: "ok"; content: string; filename: string; contentType: string; rawUrl: string }
  | { status: "not_found" }
  | { status: "no_access" }
  | { status: "error" };

/// No host prefix — `apiFetch` adds `API_URL` itself.
function apiPath(source: PreviewSource): string {
  if (source.kind === "short") return `/s/${source.id}`;
  return `/api/files/${source.owner}/${source.handle}/${source.path}`;
}

export function PreviewPage({ source }: { source: PreviewSource }) {
  const [state, setState] = useState<FetchState>({ status: "loading" });
  const router = useRouter();
  const path = apiPath(source);

  const load = useCallback(async () => {
    setState({ status: "loading" });

    let response: Response;
    try {
      response = await apiFetch(path);
    } catch {
      // `apiFetch` only throws when the server is unavailable.
      setState({ status: "error" });
      return;
    }

    if (response.status === 401) {
      // Bouncing every 401 to /login causes a loop: the login page sees the
      // session, redirects back, the fetch 401s again. Token present means
      // "logged in but no access"; absent means "not logged in".
      if (getAccessToken()) {
        setState({ status: "no_access" });
      } else {
        const here = typeof window !== "undefined" ? window.location.pathname + window.location.search : "";
        router.replace(`/login?redirect=${encodeURIComponent(here)}`);
      }
      return;
    }

    if (response.status === 404) {
      setState({ status: "not_found" });
      return;
    }

    if (!response.ok) {
      setState({ status: "error" });
      return;
    }

    const content = await response.text();
    const contentType = response.headers.get("Content-Type")?.split(";")[0].trim() ?? "text/plain";
    // `response.url` is the final URL after redirects (presigned for public
    // shares, canonical otherwise) — usable as the filename fallback.
    const filename = fileResponseFilename(response, path);
    // Anchor `href` is a real navigation, so it needs the backend host —
    // `apiFetch` adds that for us but the anchor bypasses it.
    const apiUrl = process.env.NEXT_PUBLIC_FRONA_SERVER_BACKEND_URL || "";
    setState({ status: "ok", content, filename, contentType, rawUrl: `${apiUrl}${path}` });
  }, [path, router]);

  useEffect(() => {
    void load();
  }, [load]);

  if (state.status === "loading") {
    return (
      <div className="min-h-screen flex items-center justify-center bg-surface text-text-tertiary text-sm">
        Loading…
      </div>
    );
  }

  if (state.status === "not_found") {
    return (
      <div className="min-h-screen flex items-center justify-center bg-surface">
        <div className="max-w-md text-center px-6">
          <h1 className="text-lg font-medium text-text-primary">This link is no longer available</h1>
          <p className="mt-2 text-sm text-text-secondary">
            The share may have expired or been deleted.
          </p>
        </div>
      </div>
    );
  }

  if (state.status === "no_access") {
    return (
      <div className="min-h-screen flex items-center justify-center bg-surface">
        <div className="max-w-md text-center px-6">
          <h1 className="text-lg font-medium text-text-primary">You don&rsquo;t have access to this file</h1>
          <p className="mt-2 text-sm text-text-secondary">
            Your account is signed in but doesn&rsquo;t have permission to view this content.
          </p>
        </div>
      </div>
    );
  }

  if (state.status === "error") {
    return (
      <div className="min-h-screen flex items-center justify-center bg-surface">
        <div className="max-w-md text-center px-6">
          <h1 className="text-lg font-medium text-text-primary">Couldn&rsquo;t load</h1>
          <p className="mt-2 text-sm text-text-secondary">Something went wrong fetching this file.</p>
          <button
            onClick={() => void load()}
            className="mt-4 inline-flex items-center gap-1.5 rounded-md bg-surface-tertiary px-3 py-1.5 text-xs text-text-secondary hover:text-text-primary hover:bg-surface-secondary transition-colors"
          >
            Retry
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="min-h-screen flex flex-col bg-surface">
      <header className="flex items-center justify-between gap-3 px-4 py-3 border-b border-border">
        <div className="flex items-center gap-2 min-w-0">
          <span aria-hidden className="text-base">📄</span>
          <span className="text-sm font-medium text-text-primary truncate">{state.filename}</span>
        </div>
        <FileDownloadButton
          url={state.rawUrl}
          filename={state.filename}
          className="flex items-center gap-1.5 rounded-md bg-surface-tertiary px-2.5 py-1.5 text-xs text-text-secondary hover:text-text-primary hover:bg-surface-secondary transition-colors"
        >
          <ArrowDownTrayIcon className="h-3.5 w-3.5" />
          Download
        </FileDownloadButton>
      </header>
      <main className="flex-1 overflow-auto p-4 sm:p-6">
        <div className="mx-auto max-w-3xl">
          <FilePreviewContent
            content={state.content}
            filename={state.filename}
            contentType={state.contentType}
          />
        </div>
      </main>
    </div>
  );
}
