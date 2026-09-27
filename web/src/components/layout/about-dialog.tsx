"use client";

import { useEffect, useState } from "react";
import { InformationCircleIcon } from "@heroicons/react/24/outline";
import { API_URL, getAccessToken } from "@/lib/api-client";
import { Dialog } from "@/components/dialog";

interface SystemInfo {
  version: string;
  revision?: string | null;
  cpus: number;
  total_memory_bytes: number;
  sandbox_driver: string;
}

function formatBytes(bytes: number): string {
  const gb = bytes / 1_073_741_824;
  return gb >= 1 ? `${gb.toFixed(1)} GB` : `${(bytes / 1_048_576).toFixed(0)} MB`;
}

const SANDBOX_LABELS: Record<string, string> = {
  macos: "MacOS",
  syd: "Syd",
  landlock: "Landlock",
  disabled: "Disabled",
};

interface AboutDialogProps {
  open: boolean;
  onClose: () => void;
}

export function AboutDialog({ open, onClose }: AboutDialogProps) {
  const [info, setInfo] = useState<SystemInfo | null>(null);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch(`${API_URL}/api/system/info`, {
          headers: { Authorization: `Bearer ${getAccessToken()}` },
          credentials: "include",
        });
        if (res.ok && !cancelled) setInfo(await res.json());
      } catch {}
    })();
    return () => { cancelled = true; };
  }, [open]);

  const row = (label: string, value: string) => (
    <div className="flex items-center justify-between gap-4 py-2 border-b border-border last:border-b-0">
      <span className="text-xs font-medium text-text-tertiary">{label}</span>
      <span className="text-sm text-text-primary">{value}</span>
    </div>
  );

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="About"
      description="System information"
      icon={InformationCircleIcon}
    >
      <div>
        {row("Version", info ? `${info.version}${info.revision ? ` (${info.revision.slice(0, 12)})` : ""}` : "…")}
        {row("CPUs", info ? `${info.cpus} cores` : "…")}
        {row("Memory", info ? formatBytes(info.total_memory_bytes) : "…")}
        {row("Sandbox", info ? (SANDBOX_LABELS[info.sandbox_driver] ?? info.sandbox_driver) : "…")}
      </div>
      <div className="flex gap-2 pt-4">
        <button
          onClick={onClose}
          className="w-32 inline-flex items-center justify-center gap-1.5 rounded-lg border border-border px-4 py-2 text-sm font-medium text-text-secondary hover:bg-surface-tertiary transition"
        >
          Close
        </button>
      </div>
    </Dialog>
  );
}
