import React, { useState } from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { ErrorBoundaryHandler } from "next/dist/client/components/error-boundary";
import MainError from "../error";

const failure = new Error("Private server details should only be logged");
let shouldFail = true;
function Route({ savedMessages }: { savedMessages: string[] }) {
  if (shouldFail) throw failure;
  return <div>{savedMessages.map(message => <p key={message}>{message}</p>)}</div>;
}
function Layout() {
  const [workspace, setWorkspace] = useState("Existing workspace state");
  // Models state in main layout providers, above Next's route boundary.
  const [savedMessages] = useState(["Saved user message", "Saved assistant answer"]);
  return <>
    <input aria-label="Workspace state" value={workspace} onChange={event => setWorkspace(event.target.value)} />
    <ErrorBoundaryHandler pathname="/chat" errorComponent={MainError}>
      <Route savedMessages={savedMessages} />
    </ErrorBoundaryHandler>
  </>;
}

beforeEach(() => {
  shouldFail = true;
  vi.spyOn(console, "error").mockImplementation(() => {});
});
afterEach(() => vi.restoreAllMocks());

it("logs route failures and retries through the installed Next boundary without resetting parent data", async () => {
  render(<Layout />);
  expect(screen.getByRole("alert")).toHaveTextContent("Unable to display this page");
  expect(screen.queryByText(failure.message)).not.toBeInTheDocument();
  expect(console.error).toHaveBeenCalledWith("Route rendering failed.", failure);
  fireEvent.change(screen.getByRole("textbox", { name: "Workspace state" }), { target: { value: "Still usable" } });
  shouldFail = false;
  fireEvent.click(screen.getByRole("button", { name: "Try again" }));
  await waitFor(() => expect(screen.queryByRole("alert")).not.toBeInTheDocument());
  expect(screen.getByRole("textbox")).toHaveValue("Still usable");
  expect(screen.getByText("Saved user message")).toBeInTheDocument();
  expect(screen.getByText("Saved assistant answer")).toBeInTheDocument();
});

it("keeps retry available if the route still fails, then recovers when fixed", async () => {
  render(<Layout />);
  fireEvent.click(screen.getByRole("button", { name: "Try again" }));
  expect(screen.getByRole("alert")).toBeInTheDocument();
  shouldFail = false;
  fireEvent.click(screen.getByRole("button", { name: "Try again" }));
  expect(await screen.findByText("Saved user message")).toBeInTheDocument();
});

it("logs a new error when the route fallback receives one", () => {
  const reset = vi.fn();
  const view = render(<MainError error={failure} reset={reset} />);
  const nextError = new Error("Next route failure");
  view.rerender(<MainError error={nextError} reset={reset} />);
  expect(console.error).toHaveBeenCalledWith("Route rendering failed.", nextError);
  fireEvent.click(screen.getByRole("button", { name: "Try again" }));
  expect(reset).toHaveBeenCalledOnce();
});
