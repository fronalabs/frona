import { act, render, screen } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { codeToHtml } from "shiki";
import { FilePreviewContent } from "../file-preview-content";

vi.mock("shiki", () => ({ codeToHtml: vi.fn() }));
beforeEach(() => {
  vi.mocked(codeToHtml).mockRejectedValue(new Error("Highlighter unavailable"));
});

it.each([
  ["disguised.html", "text/plain"],
  ["source.rs", "application/octet-stream"],
  ["Makefile", "application/octet-stream"],
])("keeps %s inert even if syntax highlighting fails", async (filename, contentType) => {
  const content = '<img src=x onerror="alert(1)"><script>alert(1)</script>';
  await act(async () => { render(<FilePreviewContent content={content} filename={filename} contentType={contentType} />); });
  expect(codeToHtml).toHaveBeenCalled();
  expect(screen.getByText(content)).toBeInTheDocument();
  expect(document.querySelector("script, img, iframe")).toBeNull();
});
