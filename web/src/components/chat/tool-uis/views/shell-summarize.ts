import { parse } from "unbash";
import type {
  Command,
  CompoundList,
  SyntaxNode as Node,
  Word,
} from "unbash";

const MAX_LEN = 80;

// Commands that wrap a real workload behind noise (logging, pacing, scaffolding).
// When picking a representative command inside a for/while/if body, skip these
// so the title surfaces the actual work (e.g. python3, curl) instead of `echo`.
const NOISY: ReadonlySet<string> = new Set([
  "echo", "printf", "sleep", "true", "false", ":", "set", "unset", "export", "cd",
]);

/**
 * Per-command formatters. Each takes the command's argv (suffix words, joined)
 * and returns the subtitle — i.e. the salient detail. The title is always the
 * capitalized command name.
 *
 * Add new entries here when a tool has a "natural" interesting arg different
 * from the first positional (e.g. curl wants the URL, even if it comes after
 * flags; python3 wants the .py path).
 */
const FORMATTERS: Record<string, (argv: string[]) => string> = {
  curl: (a) => a.find((s) => /^https?:/.test(s)) ?? firstPositional(a) ?? "",
  wget: (a) => a.find((s) => /^https?:/.test(s)) ?? firstPositional(a) ?? "",
  git: (a) => a[0] ?? "",
  npm: (a) => a[0] ?? "",
  pnpm: (a) => a[0] ?? "",
  yarn: (a) => a[0] ?? "",
  bun: (a) => a[0] ?? "",
  cargo: (a) => a[0] ?? "",
  go: (a) => a[0] ?? "",
  docker: (a) => a[0] ?? "",
  kubectl: (a) => a[0] ?? "",
  python: (a) => a.find((s) => s.endsWith(".py")) ?? firstPositional(a) ?? "",
  python3: (a) => a.find((s) => s.endsWith(".py")) ?? firstPositional(a) ?? "",
  node: (a) =>
    a.find((s) => /\.(js|mjs|cjs|ts|tsx)$/.test(s)) ?? firstPositional(a) ?? "",
  ssh: (a) => firstPositional(a) ?? "",
  scp: (a) => firstPositional(a) ?? "",
  grep: (a) => firstPositional(a) ?? "",
  rg: (a) => firstPositional(a) ?? "",
  ripgrep: (a) => firstPositional(a) ?? "",
  fd: (a) => firstPositional(a) ?? "",
  find: (a) => firstPositional(a) ?? "",
  ls: (a) => firstPositional(a) ?? "",
  cat: (a) => firstPositional(a) ?? "",
  mv: (a) => a.filter((s) => !s.startsWith("-")).slice(0, 2).join(" "),
  cp: (a) => a.filter((s) => !s.startsWith("-")).slice(0, 2).join(" "),
  rm: (a) => firstPositional(a) ?? "",
  mkdir: (a) => firstPositional(a) ?? "",
  rmdir: (a) => firstPositional(a) ?? "",
  touch: (a) => firstPositional(a) ?? "",
  chmod: (a) => a.slice(0, 2).join(" "),
  chown: (a) => a.slice(0, 2).join(" "),
  tar: (a) => firstPositional(a) ?? a[0] ?? "",
  zip: (a) => firstPositional(a) ?? "",
  unzip: (a) => firstPositional(a) ?? "",
  make: (a) => firstPositional(a) ?? "",
  jq: (a) => firstPositional(a) ?? "",
  awk: (a) => firstPositional(a) ?? "",
  sed: (a) => firstPositional(a) ?? "",
};

export type ShellSummary = { title: string; subtitle: string };

function firstPositional(argv: string[]): string | undefined {
  return argv.find((s) => !s.startsWith("-"));
}

function wordValue(w: Word | undefined): string {
  if (!w) return "";
  return w.value || w.text || "";
}

function capitalize(s: string): string {
  if (!s) return s;
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function truncate(s: string, max = MAX_LEN): string {
  const collapsed = s.replace(/\s+/g, " ").trim();
  if (collapsed.length <= max) return collapsed;
  return collapsed.slice(0, max - 1) + "…";
}

/**
 * Walk into compound nodes (for/while/if/subshell/...) to find the underlying
 * "action" command — the first non-noisy Command we can pin a title on.
 */
function findActionCommand(node: Node): Command | null {
  switch (node.type) {
    case "Command":
      return node;
    case "Statement":
    case "Redirected":
      return findActionCommand(node.command);
    case "For":
    case "While":
    case "Subshell":
    case "BraceGroup":
    case "Select":
      return findActionInList(node.body);
    case "If":
      return findActionInList(node.then);
    case "CompoundList":
      return findActionInList(node);
    default:
      return null;
  }
}

function findActionInList(list: CompoundList): Command | null {
  // First pass: skip noisy commands so we find the real action.
  for (const stmt of list.commands) {
    const cmd = findActionCommand(stmt.command);
    if (!cmd) continue;
    const name = wordValue(cmd.name);
    if (name && !NOISY.has(name)) return cmd;
  }
  // Second pass: accept anything.
  for (const stmt of list.commands) {
    const cmd = findActionCommand(stmt.command);
    if (cmd) return cmd;
  }
  return null;
}

function summarizeSingleCommand(cmd: Command): ShellSummary {
  const name = wordValue(cmd.name);
  if (!name) {
    // Assignment-only statement: `FOO=bar`
    const assigns = cmd.prefix.filter((p) => p.type === "Assignment").map((p) => p.text).join(" ");
    return { title: "Shell", subtitle: truncate(assigns) };
  }
  const argv = cmd.args.map((arg) => arg.type === "Word" ? wordValue(arg) : arg.text).filter((s) => s.length > 0);
  const formatter = FORMATTERS[name];
  const subtitleRaw = formatter ? formatter(argv) : firstPositional(argv) ?? "";
  return {
    title: capitalize(name),
    subtitle: truncate(subtitleRaw),
  };
}

/**
 * Render a compound (pipeline / and-or) as a flat string for the subtitle.
 * Each leaf command is rendered as `name arg` using its formatter.
 */
function flattenCompound(node: Node): string {
  switch (node.type) {
    case "Command": {
      const { title, subtitle } = summarizeSingleCommand(node);
      const lower = title.toLowerCase();
      return subtitle ? `${lower} ${subtitle}` : lower;
    }
    case "Statement":
    case "Redirected":
      return flattenCompound(node.command);
    case "Pipeline": {
      const parts = node.commands.map(flattenCompound).filter(Boolean);
      const op = node.operators[0] ?? "|";
      return parts.join(` ${op} `);
    }
    case "AndOr": {
      const parts = node.commands.map(flattenCompound).filter(Boolean);
      const op = node.operators[0] ?? "&&";
      if (parts.length <= 2) return parts.join(` ${op} `);
      return `${parts[0]} ${op} ${parts[1]} ${op} …`;
    }
    case "For":
      return `for ${wordValue(node.name)}: ${flattenCompoundList(node.body)}`;
    case "While":
      return `${node.kind}: ${flattenCompoundList(node.body)}`;
    case "If":
      return `if: ${flattenCompoundList(node.then)}`;
    case "Subshell":
      return `(${flattenCompoundList(node.body)})`;
    case "BraceGroup":
      return `{ ${flattenCompoundList(node.body)} }`;
    case "Case":
      return `case ${wordValue(node.word)}`;
    case "Function":
      return `function ${wordValue(node.name)}`;
    case "TestCommand":
      return "[[ … ]]";
    case "ArithmeticCommand":
    case "ArithmeticFor":
      return "((…))";
    default:
      return node.type;
  }
}

function flattenCompoundList(list: CompoundList): string {
  if (!list.commands.length) return "";
  const first = list.commands[0];
  return flattenCompound(first.command);
}

/**
 * Produce a `{ title, subtitle }` pair from a shell command:
 *
 *   curl https://api.example.com           → { title: "Curl",     subtitle: "https://api.example.com" }
 *   git commit -m "msg"                    → { title: "Git",      subtitle: "commit" }
 *   for i in 1..10; do python3 a.py; done  → { title: "Python3",  subtitle: "a.py" }
 *   find . | grep foo                      → { title: "Shell",    subtitle: "find . | grep foo" }
 *
 * Compound commands (pipelines, &&/||, multi-statement scripts) fall back to
 * `title: "Shell"` with the full flat summary in the subtitle. Control-flow
 * wrappers (for/while/if/subshell) "unwrap" to the action command inside.
 *
 * Falls back to `title: "Shell"` and a whitespace-collapsed truncation of the
 * raw command when parsing yields nothing.
 */
export function summarizeCommand(command: string): ShellSummary {
  const raw = command.trim();
  if (!raw) return { title: "Shell", subtitle: "" };

  let ast: ReturnType<typeof parse>;
  try {
    ast = parse(raw);
  } catch {
    return { title: "Shell", subtitle: truncate(raw) };
  }

  const stmts = ast.commands;
  if (!stmts.length) return { title: "Shell", subtitle: truncate(raw) };

  // Multiple top-level statements (separated by `;` or newlines): treat as a
  // script. Pull a representative action command for the title; subtitle gets
  // the first action's flat summary.
  if (stmts.length > 1) {
    const action = findActionInList({
      type: "CompoundList",
      pos: 0,
      end: 0,
      commands: stmts,
    });
    if (action) {
      const { title, subtitle } = summarizeSingleCommand(action);
      const tail = "; …";
      return { title, subtitle: truncate(subtitle + tail) };
    }
    return { title: "Shell", subtitle: truncate(raw) };
  }

  const top = stmts[0].command;

  // Pipelines and &&/|| stay under "Shell" — there's no single verb that
  // captures a multi-stage chain.
  if (top.type === "Pipeline" || top.type === "AndOr") {
    return { title: "Shell", subtitle: truncate(flattenCompound(top)) };
  }

  // Control flow (for/while/if/subshell/etc.) unwraps to the body's action.
  const action = findActionCommand(top);
  if (action) return summarizeSingleCommand(action);

  return { title: "Shell", subtitle: truncate(raw) };
}
