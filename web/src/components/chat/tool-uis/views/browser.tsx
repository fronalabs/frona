"use client";

import { useEffect, useRef, useState } from "react";
import { apiFetch } from "@/lib/api-client";
import { useToolTimeline } from "../tool-timeline-context";
import { browserNavigationWarnings } from "../browser-tool-state";
import { ToolRow } from "./tool-row";
import type { ToolView } from "./types";

function useBrowserSnapshot(revision: string, closed: boolean) {
  const [image, setImage] = useState<string>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const imageRef = useRef<string | undefined>(undefined);

  useEffect(() => {
    const controller = new AbortController();
    function replaceImage(next?: string) {
      if (imageRef.current) URL.revokeObjectURL(imageRef.current);
      imageRef.current = next;
      setImage(next);
    }
    setError(undefined);
    if (closed || !revision) {
      if (closed) replaceImage();
      setLoading(false);
      return;
    }
    setLoading(true);
    apiFetch("/api/browser/snapshot", { signal: controller.signal, cache: "no-store" })
      .then(async (response) => {
        if (controller.signal.aborted) return;
        if (response.status === 204) {
          replaceImage();
          setError("No active browser page");
          return;
        }
        if (!response.ok) throw new Error("Snapshot unavailable");
        const blob = await response.blob();
        if (!controller.signal.aborted) replaceImage(URL.createObjectURL(blob));
      })
      .catch(() => {
        if (!controller.signal.aborted) setError("Snapshot unavailable. The next browser action will try again.");
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [revision, closed]);

  useEffect(() => () => {
    if (imageRef.current) URL.revokeObjectURL(imageRef.current);
  }, []);

  return { image, loading, error };
}

export const BrowserView: ToolView = (props) => {
  const { toolName, toolCallId, args, result, status, isExpanded, onToggle } = props;
  const timeline = useToolTimeline();
  const calls = timeline?.browserCalls.length ? timeline.browserCalls : [props];
  // IDs distinguish repeated calls and empty results; text streaming cannot trigger captures.
  const completed = calls.filter((call) => call.result !== undefined);
  const revision = completed.map((call) => call.toolCallId).join(",");
  const lastCompleted = completed.at(-1);
  const navigation = completed.filter((call) => call.toolName === "browser_navigate").at(-1);
  const warnings = browserNavigationWarnings(navigation?.result);
  const closed = lastCompleted?.toolName === "browser_close" && lastCompleted.args?.isError !== true;
  const { image, loading, error } = useBrowserSnapshot(revision, closed);
  const action = toolName.replace(/^browser_/, "").replace(/_/g, " ");
  const url = typeof args?.url === "string" ? args.url : undefined;
  const failed = status?.type === "incomplete";
  const failure = failed
    ? (status.reason === "cancelled" ? "Browser action cancelled" : typeof result === "string" && result ? result : "Browser action failed")
    : undefined;
  const preview = (
    <div className="flex flex-col gap-2" aria-busy={loading || status?.type === "running"}>
      {url && <p className="truncate text-xs text-text-tertiary" title={url}>{url}</p>}
      {image ? (
        <a href={image} target="_blank" rel="noopener noreferrer" aria-label="Open browser snapshot">
          <img src={image} alt="Current browser page" className="max-h-80 w-full rounded border border-border object-contain bg-white" />
        </a>
      ) : (
        <div className="flex h-40 items-center justify-center rounded bg-surface-tertiary text-xs text-text-tertiary">
          {closed ? "Browser closed" : error ?? (loading ? "Loading browser snapshot…" : "Waiting for the browser…")}
        </div>
      )}
      <div className="text-xs text-text-tertiary" role="status">
        {image && (error ?? (loading ? "Refreshing snapshot…" : "Current browser snapshot"))}
        {failure && <p className="text-danger">{failure}</p>}
        {!closed && warnings.length > 0 && (
          <div>
            <p className="text-warning">Page ready with warnings</p>
            {warnings.map((warning, index) => <p key={index}>{warning}</p>)}
          </div>
        )}
      </div>
      <details key={toolCallId} className="text-xs text-text-tertiary">
        <summary className="cursor-pointer">Action details{calls.length > 1 ? ` (${calls.length} calls)` : ""}</summary>
        {calls.map((call) => (
          <div key={call.toolCallId} className="mt-2">
            <p className="font-medium">{call.toolName.replace(/^browser_/, "").replace(/_/g, " ")}</p>
            <pre className="max-h-40 overflow-auto whitespace-pre-wrap break-words">{typeof call.result === "string" ? call.result : JSON.stringify(call.result, null, 2)}</pre>
          </div>
        ))}
      </details>
    </div>
  );

  return (
    <ToolRow status={status}>
      <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
        <ToolRow.Title>Browser</ToolRow.Title>
        <ToolRow.Subtitle>{action}{status?.type === "running" ? "…" : ""}</ToolRow.Subtitle>
      </ToolRow.Header>
      <ToolRow.Error>{preview}</ToolRow.Error>
      <ToolRow.Body isExpanded={isExpanded}>{preview}</ToolRow.Body>
    </ToolRow>
  );
};
