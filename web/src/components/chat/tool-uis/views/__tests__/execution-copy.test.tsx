import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CodeBlock } from "@/components/ui/code-block";
import { ShellView } from "../shell";
import { PythonView } from "../python";
import { NodeView } from "../node";
import { mkProps } from "./helpers";

// Keep real CodeBlock + CopyButton; highlighting is unrelated to clipboard content.
vi.mock("shiki", () => ({ codeToHtml: vi.fn(async () => "") }));
const writeText = vi.fn(async () => {});
beforeEach(() => {
  writeText.mockClear();
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  vi.useFakeTimers();
});
afterEach(() => { cleanup(); vi.clearAllTimers(); vi.useRealTimers(); });

const denial = '{"ctx":"access","cap":"net/connect","act":"deny","sys":"connect","addr":"1.2.3.4!80","pid":42,"tip":"allow only this endpoint"}';
const cases = [
  { name: "command only", result: undefined, output: "" },
  { name: "successful output", result: "hello\n", output: "\n\nOutput:\nhello\n" },
  { name: "empty output", result: "", output: "" },
  { name: "error only", result: undefined, error: "permission denied", output: "\n\nError:\npermission denied" },
  { name: "output and error", result: "started\n", error: "permission denied", output: "\n\nOutput:\nstarted\n\n\nError:\npermission denied" },
  { name: "duplicate error", result: "permission denied\n", error: "permission denied", output: "\n\nOutput:\npermission denied\n" },
  { name: "error already in output", result: "started\npermission denied\n", error: "permission denied", output: "\n\nOutput:\nstarted\npermission denied\n" },
  { name: "duplicate multiline error", result: "started\nTraceback:\n  denied\n", error: "Traceback:\n  denied", output: "\n\nOutput:\nstarted\nTraceback:\n  denied\n" },
  { name: "duplicate CRLF error line", result: "started\r\npermission denied\r\nfinished", error: "permission denied", output: "\n\nOutput:\nstarted\r\npermission denied\r\nfinished" },
  { name: "duplicate CRLF error block", result: "started\r\nTraceback:\r\n  denied\r\nfinished", error: "Traceback:\n  denied", output: "\n\nOutput:\nstarted\r\nTraceback:\r\n  denied\r\nfinished" },
  { name: "distinct substring error", result: "not denied", error: "denied", output: "\n\nOutput:\nnot denied\n\nError:\ndenied" },
  { name: "sandbox denial", result: `before\n${denial}\n${denial}\nafter\n`, error: "blocked", output: `\n\nOutput:\nbefore\n${denial}\n${denial}\nafter\n\n\nError:\nblocked` },
];

describe.each([
  { name: "shell", View: ShellView, argKey: "command", code: "echo hello" },
  { name: "python", View: PythonView, argKey: "code", code: "print('hello')" },
  { name: "node", View: NodeView, argKey: "code", code: "console.log('hello')" },
])("$name transcript copy", ({ name, View, argKey, code }) => {
  it.each(cases)("copies $name through the real clipboard button", async ({ result, error, output }) => {
    await act(async () => { render(<View {...mkProps({ toolName: name, args: { [argKey]: code }, result, ...(error ? { status: { type: "incomplete", reason: "error", error } } : {}) })} />); });
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledExactlyOnceWith(`${code}${output}`);
    expect(screen.getByRole("button", { name: "Copied" })).toBeInTheDocument();
    act(() => vi.advanceTimersByTime(2000));
    expect(screen.getByRole("button", { name: "Copy" })).toBeInTheDocument();
  });

  it("copies an error when the command is unavailable", async () => {
    await act(async () => { render(<View {...mkProps({ toolName: name, args: {}, status: { type: "incomplete", reason: "error", error: "command unavailable" } })} />); });
    expect(screen.getByRole("button", { name: new RegExp(name, "i") })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledExactlyOnceWith("Error:\ncommand unavailable");
  });

  it("includes structured error details once when output already contains them", async () => {
    const details = { message: "denied", path: "/private/file", capability: "fs/read" };
    await act(async () => { render(<View {...mkProps({ toolName: name, args: { [argKey]: code }, result: details, status: { type: "incomplete", reason: "error", error: details } })} />); });
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledExactlyOnceWith(`${code}\n\nOutput:\n${JSON.stringify(details, null, 2)}`);
  });

  it("copies the latest output after the execution updates", async () => {
    const view = render(<View {...mkProps({ toolName: name, args: { [argKey]: code }, result: "started" })} />);
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    await act(async () => { view.rerender(<View {...mkProps({ toolName: name, args: { [argKey]: code }, result: "finished" })} />); });
    fireEvent.click(screen.getByRole("button", { name: /^Cop/ }));
    expect(writeText).toHaveBeenLastCalledWith(`${code}\n\nOutput:\nfinished`);
  });

  it("copies output when the command is unavailable", async () => {
    await act(async () => { render(<View {...mkProps({ toolName: name, args: {}, result: "completed" })} />); });
    fireEvent.click(screen.getByRole("button", { name: "Copy" }));
    expect(writeText).toHaveBeenCalledExactlyOnceWith("Output:\ncompleted");
  });
});

it("keeps ordinary code block copying unchanged", async () => {
  const code = "  const exact = 'text';\n";
  await act(async () => { render(<CodeBlock code={code} language="javascript" />); });
  fireEvent.click(screen.getByRole("button", { name: "Copy" }));
  expect(writeText).toHaveBeenCalledExactlyOnceWith(code);
});
