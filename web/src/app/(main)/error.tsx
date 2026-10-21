"use client";

import { useEffect } from "react";

export default function MainError({ error, reset }: { error: unknown; reset: () => void }) {
  useEffect(() => {
    console.error("Route rendering failed.", error);
  }, [error]);

  return (
    <div role="alert" className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
      <h2 className="text-lg font-semibold">Unable to display this page</h2>
      <p className="text-sm text-text-secondary">Try again to restore this view.</p>
      <button type="button" onClick={reset}
        className="rounded-lg bg-accent px-4 py-2 text-sm font-medium text-surface focus-visible:outline-2 focus-visible:outline-accent">
        Try again
      </button>
    </div>
  );
}
