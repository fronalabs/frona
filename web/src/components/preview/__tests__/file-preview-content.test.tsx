import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FilePreviewContent } from "../file-preview-content";

vi.mock("@/components/ui/code-block", () => ({ CodeBlock: ({ code }: { code: string }) => <pre>{code}</pre> }));
afterEach(cleanup);

describe("file preview content", () => {
  it.each([
    ["dashboard.html", "text/html"],
    ["dashboard.HTM", "application/octet-stream"],
    ["dashboard", "text/html; charset=utf-8"],
    ["dashboard.xhtml", "application/xhtml+xml"],
  ])("isolates %s while retaining dashboard scripts and embedded assets", (filename, contentType) => {
    const html = '<button onclick="count++">Count</button><script>let count=0</script><script src="https://cdn.example/chart.js"></script><img src="data:image/png;base64,AA==">';
    render(<FilePreviewContent content={html} filename={filename} contentType={contentType} />);
    const frame = screen.getByTitle(filename);
    expect(frame.tagName).toBe("IFRAME");
    expect(frame).toHaveAttribute("sandbox", "allow-scripts");
    expect(frame).toHaveAttribute("srcdoc", html);
    expect(frame).toHaveAttribute("referrerpolicy", "no-referrer");
    expect(document.querySelector("script")).toBeNull();
    // jsdom cannot enforce browser isolation; real-browser checks cover it.
  });

  it("keeps Markdown rendering and treats embedded HTML as text", () => {
    render(<FilePreviewContent content={'# Notes\n<script>alert(1)</script>'} filename="notes.md" contentType="text/markdown" />);
    expect(screen.getByRole("heading", { name: "Notes" })).toBeInTheDocument();
    expect(document.querySelector("script, iframe")).toBeNull();
  });

  it("keeps plain text inert", () => {
    render(<FilePreviewContent content="<script>alert(1)</script>" filename="notes.txt" contentType="text/plain" />);
    expect(screen.getByText("<script>alert(1)</script>")).toBeInTheDocument();
    expect(document.querySelector("script, iframe")).toBeNull();
  });
});
