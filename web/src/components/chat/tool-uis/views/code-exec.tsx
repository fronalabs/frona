"use client";

import type { FC } from "react";
import { LockClosedIcon } from "@heroicons/react/24/solid";
import { CodeBlock } from "@/components/ui/code-block";
import { CopyButton } from "@/components/ui/copy-button";
import { cn } from "@/lib/utils";
import { ToolRow } from "./tool-row";
import { bestSeverity, parseShellOutput } from "./shell-sandbox";
import { SandboxBlock } from "./shell-sandbox-block";
import type { ToolView, ToolViewProps } from "./types";

const SANDBOX_ICON_CLASS = {
  low: "text-text-tertiary",
  high: "text-warning",
  critical: "text-danger",
} as const;

export interface CodeExecConfig {
  /** Default title; can be overridden per-call by `summarize`. */
  title: string;
  /** CodeBlock language hint (bash, python, javascript). */
  language: string;
  /** Pass `wrap` to CodeBlock — true for shell-style long commands, false for code. */
  wrap?: boolean;
  /** Show line numbers in the code block. */
  lineNumbers?: boolean;
  /** Args key holding the code/command (`"command"` for shell, `"code"` for python/node). */
  argKey: string;
  /** Failure label rendered in red ("Command failed", "Python failed"). */
  failureLabel: string;
  /**
   * Optional summarizer to produce a smart title/subtitle from the code.
   * Falls back to {config.title, firstLine(code)} when not provided.
   */
  summarize?: (code: string) => { title: string; subtitle: string };
}

export function makeCodeExecView(config: CodeExecConfig): ToolView {
  const Component: FC<ToolViewProps> = ({
    toolName,
    args,
    result,
    status,
    isExpanded,
    onToggle,
  }) => {
    const a = (args && typeof args === "object" ? args : {}) as Record<string, unknown>;
    const code =
      typeof a[config.argKey] === "string" ? (a[config.argKey] as string) : "";

    const resultText =
      typeof result === "string"
        ? result
        : result !== undefined
          ? JSON.stringify(result, null, 2)
          : "";

    const errorText =
      status?.type === "incomplete"
        ? (() => {
            const e = (status as { error?: unknown }).error;
            return e == null
              ? null
              : typeof e === "string"
                ? e
                : JSON.stringify(e, null, 2);
          })()
        : null;

    // Compare whole lines/blocks, but preserve the original text in the copy.
    // In particular, sandbox parsing strips details needed to diagnose a denial.
    const outputForComparison = resultText.replaceAll("\r\n", "\n").trim();
    const errorForComparison = errorText?.replaceAll("\r\n", "\n").trim();
    const errorIncludedInOutput = errorForComparison &&
      `\n${outputForComparison}\n`.includes(`\n${errorForComparison}\n`);
    const transcript = [
      code,
      resultText && `Output:\n${resultText}`,
      errorForComparison && !errorIncludedInOutput && `Error:\n${errorText}`,
    ].filter(Boolean).join("\n\n");

    const commandBlock = code ? (
      <CodeBlock
        code={code}
        language={config.language}
        wrap={config.wrap}
        lineNumbers={config.lineNumbers}
        copyValue={transcript}
      />
    ) : transcript ? <CopyButton value={transcript} className="self-end mr-2 mt-2" /> : null;

    const { events: sandboxEvents, remainingText } = parseShellOutput(resultText);
    const sandboxSeverity = bestSeverity(sandboxEvents);

    const expandable =
      code.length > 0 || resultText.length > 0 || !!errorText;

    // Title/subtitle. Shell-like tools pass a summarizer that derives both
    // from the command itself. Code-exec tools (python/node) fall back to the
    // LLM-supplied `description` arg — when missing or equal to the tool name
    // (the runtime's default), we show no subtitle rather than invent one.
    let title: string;
    let subtitle: string;
    if (code && config.summarize) {
      ({ title, subtitle } = config.summarize(code));
    } else {
      title = config.title;
      const description =
        typeof a.description === "string" ? a.description : "";
      subtitle = description && description !== toolName ? description : "";
    }

    return (
      <ToolRow status={status} expandable={expandable}>
        <ToolRow.Header onToggle={onToggle} isExpanded={isExpanded}>
          <ToolRow.Title>{title}</ToolRow.Title>
          <ToolRow.Subtitle>{subtitle || null}</ToolRow.Subtitle>
          {sandboxSeverity && (
            <LockClosedIcon
              aria-label={`Sandbox denied ${sandboxEvents.length} action${sandboxEvents.length === 1 ? "" : "s"}`}
              className={cn(
                "inline-block h-3.5 w-3.5 ml-1.5 align-middle relative -top-px",
                SANDBOX_ICON_CLASS[sandboxSeverity],
              )}
            />
          )}
        </ToolRow.Header>

        <ToolRow.Body isExpanded={isExpanded} unstyled>
          <div className="flex flex-col gap-2">
            {commandBlock}
            {sandboxEvents.length > 0 && <SandboxBlock events={sandboxEvents} />}
            {remainingText && (
              <pre className="whitespace-pre-wrap text-text-secondary text-xs overflow-x-auto px-3 pb-3">
                {remainingText}
              </pre>
            )}
          </div>
        </ToolRow.Body>

        <ToolRow.Error>
          <div className="flex flex-col gap-2">
            {commandBlock}
            {sandboxEvents.length > 0 && <SandboxBlock events={sandboxEvents} />}
            <div className="text-xs px-3 pb-3">
              <p className="font-semibold text-danger">{config.failureLabel}</p>
              {errorText && errorText !== resultText && (
                <pre className="whitespace-pre-wrap text-text-tertiary mt-1">
                  {errorText}
                </pre>
              )}
              {remainingText && (
                <pre className="whitespace-pre-wrap text-text-tertiary mt-1">
                  {remainingText}
                </pre>
              )}
            </div>
          </div>
        </ToolRow.Error>
      </ToolRow>
    );
  };
  Component.displayName = `CodeExec(${config.title})`;
  return Component;
}
