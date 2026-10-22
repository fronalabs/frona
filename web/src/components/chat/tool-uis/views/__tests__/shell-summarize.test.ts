import { describe, it, expect } from "vitest";
import { summarizeCommand } from "../shell-summarize";

describe("summarizeCommand — single commands", () => {
  it("returns Shell/empty for empty input", () => {
    expect(summarizeCommand("")).toEqual({ title: "Shell", subtitle: "" });
    expect(summarizeCommand("   ")).toEqual({ title: "Shell", subtitle: "" });
  });

  it("Curl + URL", () => {
    expect(summarizeCommand("curl -X POST https://api.example.com -d foo")).toEqual({
      title: "Curl",
      subtitle: "https://api.example.com",
    });
  });

  it("Git + subcommand", () => {
    expect(summarizeCommand("git commit -m 'msg'")).toEqual({
      title: "Git",
      subtitle: "commit",
    });
    expect(summarizeCommand("git status")).toEqual({
      title: "Git",
      subtitle: "status",
    });
  });

  it("Grep + pattern, dropping flags", () => {
    expect(summarizeCommand("grep -rn 'foo' src/")).toEqual({
      title: "Grep",
      subtitle: "foo",
    });
  });

  it("Python3 + script path", () => {
    expect(summarizeCommand("python3 -u scripts/run.py --flag")).toEqual({
      title: "Python3",
      subtitle: "scripts/run.py",
    });
  });

  it("Node + entrypoint", () => {
    expect(summarizeCommand("node --experimental-vm-modules build/index.mjs")).toEqual({
      title: "Node",
      subtitle: "build/index.mjs",
    });
  });

  it("strips leading env assignments", () => {
    expect(summarizeCommand("FOO=bar BAR=baz python3 script.py")).toEqual({
      title: "Python3",
      subtitle: "script.py",
    });
  });

  it.each([
    ["FOO=bar > output.txt", "Shell", "FOO=bar"],
    ["cat < input.txt report.txt", "Cat", "report.txt"],
    ["export FOO=bar", "Export", "FOO=bar"],
    ["{ python3 run.py; } > output.txt", "Python3", "run.py"],
  ])("summarizes assignments and redirects: %s", (command, title, subtitle) => {
    expect(summarizeCommand(command)).toEqual({ title, subtitle });
  });

  it("capitalizes unknown commands and uses first positional", () => {
    expect(summarizeCommand("xyzunknown arg1 arg2")).toEqual({
      title: "Xyzunknown",
      subtitle: "arg1",
    });
  });

  it("uses empty subtitle when no positional args", () => {
    expect(summarizeCommand("xyzunknown --flag")).toEqual({
      title: "Xyzunknown",
      subtitle: "",
    });
  });
});

describe("summarizeCommand — composed commands stay under Shell", () => {
  it("pipeline → Shell + flat summary", () => {
    expect(summarizeCommand("find . -name '*.ts' | grep foo")).toEqual({
      title: "Shell",
      subtitle: "find . | grep foo",
    });
  });

  it("&& chain → Shell + both sides", () => {
    expect(summarizeCommand("cd /tmp && ls -la")).toEqual({
      title: "Shell",
      subtitle: "cd /tmp && ls",
    });
  });

  it("|| chain → Shell", () => {
    expect(summarizeCommand("cmd1 || cmd2 arg")).toEqual({
      title: "Shell",
      subtitle: "cmd1 || cmd2 arg",
    });
  });

  it("deep && chain collapses with ellipsis", () => {
    expect(summarizeCommand("a && b && c && d")).toEqual({
      title: "Shell",
      subtitle: "a && b && …",
    });
  });
});

describe("summarizeCommand — control flow unwraps to the action", () => {
  it("for loop with python3 body → Python3 / script.py", () => {
    const cmd =
      'for i in $(seq -w 1 10); do echo ""; echo "=="; python3 scripts/run.py; sleep 2; done';
    expect(summarizeCommand(cmd)).toEqual({
      title: "Python3",
      subtitle: "scripts/run.py",
    });
  });

  it("for loop with only noisy body falls back to bare action", () => {
    expect(summarizeCommand("for i in 1 2 3; do echo $i; done")).toEqual({
      title: "Echo",
      subtitle: "$i",
    });
  });

  it("while loop unwraps", () => {
    expect(summarizeCommand("while true; do curl https://x; sleep 1; done")).toEqual({
      title: "Curl",
      subtitle: "https://x",
    });
  });

  it("if-statement unwraps to the then-branch action", () => {
    expect(summarizeCommand("if [ -f x ]; then python3 a.py; fi")).toEqual({
      title: "Python3",
      subtitle: "a.py",
    });
  });

  it("subshell wrapping a compound stays under Shell", () => {
    // No single action to unwrap to (cd and ls are both verbs); fall through.
    expect(summarizeCommand("(cd /tmp && ls)")).toMatchObject({
      title: "Shell",
    });
  });

  it("subshell wrapping a single action unwraps", () => {
    expect(summarizeCommand("(python3 a.py)")).toEqual({
      title: "Python3",
      subtitle: "a.py",
    });
  });
});

describe("summarizeCommand — multi-statement scripts", () => {
  it("title comes from first action, subtitle ends with '; …'", () => {
    expect(summarizeCommand("git status; ls -la")).toEqual({
      title: "Git",
      subtitle: "status; …",
    });
  });
});

describe("summarizeCommand — edge cases", () => {
  it("truncates subtitle longer than 80 chars", () => {
    const long = "echo " + "x".repeat(200);
    const out = summarizeCommand(long);
    expect(out.subtitle.length).toBeLessThanOrEqual(80);
    expect(out.subtitle.endsWith("…")).toBe(true);
  });

  it("does not throw on unparseable input", () => {
    expect(() => summarizeCommand("$(((( junk")).not.toThrow();
  });

  it("the screenshot for-loop surfaces Python3 over the noise", () => {
    const cmd =
      'for i in $(seq -w 1 10); do echo ""; echo "==="; echo " RUNNING SCRIPT $i of 10"; echo "==="; python3 scripts/${i}_*.py; if [ "$i" -lt 10 ]; then echo ""; echo "(waiting 2 seconds...)"; sleep 2; fi; done';
    const out = summarizeCommand(cmd);
    expect(out.title).toBe("Python3");
    expect(out.subtitle).toContain("scripts/");
    expect(out.subtitle.length).toBeLessThanOrEqual(80);
  });
});
