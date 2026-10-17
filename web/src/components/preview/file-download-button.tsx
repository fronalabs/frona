"use client";

import { useState, type ReactNode } from "react";
import { downloadFile } from "@/lib/file-transfer";

export function FileDownloadButton({ url, filename, className, children }: {
  url: string;
  filename: string;
  className?: string;
  children: ReactNode;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <span>
      <button
        type="button"
        className={className}
        disabled={pending}
        aria-label={`Download ${filename}`}
        onClick={async (event) => {
          event.stopPropagation();
          setPending(true);
          setError(null);
          try {
            await downloadFile(url, filename);
          } catch (error) {
            setError(error instanceof Error ? error.message : "Download failed");
          } finally {
            setPending(false);
          }
        }}
      >
        {children}
      </button>
      {error && <span role="alert" className="block text-xs text-red-500">{error}</span>}
    </span>
  );
}
