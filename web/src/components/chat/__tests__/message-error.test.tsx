import { makeMessageError } from "@/lib/__tests__/fixtures/message-error";
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { convertMessage } from "@/lib/use-chat-runtime";
import { MessageError } from "../frona-assistant-message";

const state = vi.hoisted(() => ({ message: { status: {} as unknown } }));

vi.mock("@assistant-ui/react", async (importOriginal) => ({
  ...await importOriginal<typeof import("@assistant-ui/react")>(),
  useAuiState: (selector: (value: typeof state) => unknown) => selector(state),
}));

describe("chat message errors", () => {
  it("renders a saved inference error as plain text in an alert", () => {
    const error = "The 'gpt-5.3-codex' model is not supported when using Codex with a ChatGPT account.";
    state.message.status = convertMessage({
      id: "message-1", chat_id: "chat-1", role: "agent", content: "",
      status: "failed", error: makeMessageError(error), created_at: "2026-09-14T10:10:09Z",
    })?.status;
    const view = render(<MessageError />);
    expect(screen.getByRole("alert")).toHaveTextContent(error);
    expect(view.container.querySelector("code, pre")).toBeNull();
    expect(screen.getByText("Details")).toBeInTheDocument();
    expect(screen.getByText("Retries").nextElementSibling).toHaveTextContent("0");
    expect(screen.getByText("HTTP status").nextElementSibling).toHaveTextContent("400");
    expect(view.container.querySelector("time")).toHaveAttribute("dateTime", "2026-09-14T10:10:09Z");
  });

  it("shows a fallback for old failed messages without saved details", () => {
    state.message.status = { type: "incomplete", reason: "error" };
    render(<MessageError />);
    expect(screen.getByRole("alert")).toHaveTextContent("Message processing failed.");
  });

  it.each([
    { type: "running" },
    { type: "complete", reason: "stop" },
    { type: "incomplete", reason: "cancelled" },
  ])("does not show an error for $type / $reason", (status) => {
    state.message.status = status;
    render(<MessageError />);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});
