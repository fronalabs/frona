"use client";

interface DeleteConfirmDialogProps {
  open: boolean;
  onCancel: () => void;
  onConfirm: () => void;
  title?: string;
  message?: string;
}

export function DeleteConfirmDialog({
  open,
  onCancel,
  onConfirm,
  title = "Delete conversation?",
  message = "This will permanently delete this conversation and all its messages. This action cannot be undone.",
}: DeleteConfirmDialogProps) {
  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/50" onClick={onCancel} />
      <div className="relative rounded-xl border border-border bg-surface p-6 shadow-xl max-w-sm w-full mx-4">
        <h3 className="text-sm font-semibold text-text-primary">
          {title}
        </h3>
        <p className="mt-2 text-sm text-text-secondary">
          {message}
        </p>
        <div className="mt-4 flex justify-end gap-2">
          <button
            onClick={onCancel}
            className="rounded-lg px-3 py-1.5 text-sm text-text-secondary hover:bg-surface-secondary transition"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            className="rounded-lg bg-danger px-3 py-1.5 text-sm text-white hover:bg-danger/90 transition"
          >
            Delete
          </button>
        </div>
      </div>
    </div>
  );
}
