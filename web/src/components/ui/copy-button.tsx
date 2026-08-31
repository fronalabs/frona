"use client";

import { useCallback, useState } from "react";
import { CheckIcon, ClipboardDocumentIcon } from "@heroicons/react/24/outline";
import { cn } from "@/lib/utils";

export function CopyButton({
  value,
  className,
  size = "default",
}: {
  value: string;
  className?: string;
  size?: "default" | "compact";
}) {
  const [copied, setCopied] = useState(false);
  const buttonSize = size === "compact" ? "h-5 w-5 rounded" : "h-7 w-7 rounded-md";
  const iconSize = size === "compact" ? "h-3 w-3" : "h-4 w-4";

  const handleCopy = useCallback(() => {
    void navigator.clipboard.writeText(value);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }, [value]);

  return (
    <button
      type="button"
      onClick={handleCopy}
      aria-label={copied ? "Copied" : "Copy"}
      title={copied ? "Copied" : "Copy"}
      className={cn(
        "flex items-center justify-center bg-surface-tertiary/80 text-text-secondary transition-all hover:bg-surface-tertiary hover:text-text-primary",
        buttonSize,
        className,
      )}
    >
      {copied ? (
        <CheckIcon className={`${iconSize} text-[#4fd1c5]`} />
      ) : (
        <ClipboardDocumentIcon className={iconSize} />
      )}
    </button>
  );
}
