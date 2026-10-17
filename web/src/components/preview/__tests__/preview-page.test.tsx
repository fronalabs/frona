import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PreviewPage, type PreviewSource } from "../preview-page";
import { setAccessToken } from "@/lib/api-client";

vi.mock("next/navigation", () => {
  const router = { replace: vi.fn() };
  return { useRouter: () => router };
});
vi.mock("@/components/ui/code-block", () => ({ CodeBlock: ({ code }: { code: string }) => <pre>{code}</pre> }));
const fetchMock = vi.fn();
beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  setAccessToken("private-token");
});
afterEach(() => {
  cleanup();
  fetchMock.mockReset();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  setAccessToken(null);
});

// The standalone page must keep auth for Download too: ordinary navigation
// cannot send the in-memory access token used to load its private preview.
describe("standalone preview", () => {
  it.each<[PreviewSource, string]>([
    [{ kind: "short", id: "share-id" }, "/s/share-id"],
    [{ kind: "long", owner: "user", handle: "alice", path: "dashboard.html" }, "/api/files/user/alice/dashboard.html"],
  ])("retains private authorization for preview and Download (%j)", async (source, path) => {
    fetchMock.mockImplementation(async () => new Response("<button>Count</button><script>let count=0</script>", {
      headers: { "Content-Type": "text/html", "Content-Disposition": 'inline; filename="dashboard.html"' },
    }));
    vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn().mockReturnValue("blob:http://localhost/test"), revokeObjectURL: vi.fn() }));
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    render(<PreviewPage source={source} />);
    expect(await screen.findByTitle("dashboard.html")).toHaveAttribute("sandbox", "allow-scripts");
    fireEvent.click(screen.getByRole("button", { name: "Download dashboard.html" }));
    await waitFor(() => expect(click).toHaveBeenCalledOnce());
    expect(fetchMock).toHaveBeenCalledWith(`${path}?download=true`, expect.objectContaining({
      headers: { Authorization: "Bearer private-token" }, credentials: "include",
    }));
  });
  it("shows download failure without navigating away from the preview", async () => {
    fetchMock.mockResolvedValueOnce(new Response("<p>Dashboard</p>", {
      headers: { "Content-Type": "text/html", "Content-Disposition": 'inline; filename="dashboard.html"' },
    })).mockResolvedValue(new Response("Forbidden", { status: 403 }));
    render(<PreviewPage source={{ kind: "short", id: "share-id" }} />);
    fireEvent.click(await screen.findByRole("button", { name: "Download dashboard.html" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Download failed (403)");
    expect(screen.getByTitle("dashboard.html")).toBeInTheDocument();
  });
});
