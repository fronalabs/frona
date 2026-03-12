"use client";

import { useRef, useCallback, forwardRef, useImperativeHandle } from "react";

const MAX_HEIGHT = 200;

export interface AutoResizeTextareaHandle {
  focus: () => void;
  resetHeight: () => void;
}

export const AutoResizeTextarea = forwardRef<
  AutoResizeTextareaHandle,
  React.TextareaHTMLAttributes<HTMLTextAreaElement>
>(function AutoResizeTextarea({ onChange, className, ...props }, ref) {
  const taRef = useRef<HTMLTextAreaElement>(null);

  useImperativeHandle(ref, () => ({
    focus: () => taRef.current?.focus(),
    resetHeight: () => {
      if (taRef.current) taRef.current.style.height = "auto";
    },
  }));

  const resize = useCallback(() => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "auto";
    ta.style.height = `${Math.min(ta.scrollHeight, MAX_HEIGHT)}px`;
  }, []);

  const handleChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    onChange?.(e);
    resize();
  };

  return (
    <textarea
      ref={taRef}
      rows={1}
      onChange={handleChange}
      className={`resize-none overflow-y-auto ${className ?? ""}`}
      {...props}
    />
  );
});
