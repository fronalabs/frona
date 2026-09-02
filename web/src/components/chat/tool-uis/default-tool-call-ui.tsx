"use client";

import { memo, useState } from "react";
import { PuffLoader } from "react-spinners";
import {
  type ToolCallMessagePartStatus,
  type ToolCallMessagePartComponent,
} from "@assistant-ui/react";
import { AnimatePresence, motion } from "motion/react";
import ReactMarkdown from "react-markdown";
import { cn } from "@/lib/utils";
import { useToolTimeline } from "./tool-timeline-context";
import { InlineCode } from "./inline-code";
import { pickView, TOOL_VIEWS_DEFAULT_EXPANDED } from "./views";
import { SafeToolView } from "./views/safe-tool-view";

type ToolStatus = ToolCallMessagePartStatus["type"];

function TimelineDot({
  status,
  isCancelled,
  index,
}: {
  status: ToolStatus;
  isCancelled: boolean;
  index: number;
}) {
  const isRunning = status === "running";
  const isComplete = status === "complete";
  const isFailed = (status === "incomplete" && !isCancelled) || status === "requires-action";

  return (
    <div className="absolute left-0 top-[-3px] z-10 h-6 w-6 flex items-center justify-center">
      <AnimatePresence mode="wait" initial={false}>
        {isRunning ? (
          <motion.div
            key="loader"
            initial={{ opacity: 0, scale: 0.5 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.5 }}
            transition={{ duration: 0.2 }}
          >
            <PuffLoader
              color="var(--text-tertiary)"
              size={24}
              speedMultiplier={0.8}
            />
          </motion.div>
        ) : (
          <motion.div
            key="number"
            initial={{ opacity: 0, scale: 0.5 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.5 }}
            transition={{ duration: 0.2 }}
            className={cn(
              "flex h-6 w-6 items-center justify-center rounded-full text-[10px] font-semibold leading-none",
              isComplete && "bg-success/20 text-success",
              isFailed && "bg-danger/20 text-danger",
              isCancelled && "bg-surface-tertiary text-text-tertiary",
            )}
          >
            {index}
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

const ToolFallbackImpl: ToolCallMessagePartComponent = (props) => {
  const { toolName, toolCallId, args, result, status } = props;
  const timeline = useToolTimeline();
  const [isOpen, setIsOpen] = useState(
    () => TOOL_VIEWS_DEFAULT_EXPANDED[toolName]?.(args) ?? false,
  );

  if (timeline && !timeline.isVisible(toolCallId)) return null;

  // Normalize args.isError === true (server-reported failure) into a synthetic
  // incomplete-status so per-tool views only need to inspect `status` to know
  // a call failed, regardless of whether the SDK or the server reported it.
  const isToolError =
    typeof args === "object" &&
    args !== null &&
    (args as { isError?: unknown }).isError === true;
  const effectiveStatus: ToolCallMessagePartStatus | undefined =
    isToolError && status?.type !== "incomplete"
      ? { type: "incomplete", reason: "error", error: result }
      : status;

  const isCancelled =
    effectiveStatus?.type === "incomplete" && effectiveStatus.reason === "cancelled";
  const statusType: ToolStatus = effectiveStatus?.type ?? "complete";
  const isLast = timeline ? timeline.isLastVisible(toolCallId) : false;
  const isFirst = timeline ? timeline.isFirstVisible(toolCallId) : false;
  const toolIndex = timeline ? timeline.getToolIndex(toolCallId) : 0;
  const hiddenCount = timeline?.hiddenCount ?? 0;

  const turnText =
    typeof (args as { turnText?: unknown })?.turnText === "string"
      ? (args as { turnText: string }).turnText
      : null;

  const viewProps = {
    ...props,
    status: effectiveStatus,
    isExpanded: isOpen,
    onToggle: () => setIsOpen((v) => !v),
  };
  const View = pickView(toolName);

  return (
    <>
      {isFirst && hiddenCount > 0 && (
        <div className="relative pl-8 pb-3 mt-3 flex items-center min-h-6">
          <div className="absolute left-[11px] top-[21px] bottom-0 w-px bg-border" />
          <div className="absolute left-0 z-10 flex h-6 w-6 items-center justify-center rounded-full bg-surface-tertiary text-[10px] font-semibold text-text-tertiary">
            +{hiddenCount}
          </div>
          <span className="text-sm text-text-tertiary leading-none">
            tools used
          </span>
        </div>
      )}
      {turnText && (
        <div className={cn("relative pb-2 flex items-start", isFirst && hiddenCount === 0 && "mt-3")}>
          <div className="absolute left-[11px] top-0 bottom-0 w-px bg-border" />
          <div
            className="inline-block rounded-r-md bg-surface-tertiary pl-4 pr-3 py-1.5 text-xs text-text-secondary leading-none [&_p]:m-0"
            style={{ marginLeft: "11px" }}
          >
            <ReactMarkdown
              components={{
                pre: ({ children }) => <>{children}</>,
                code: ({ className, children }) => {
                  const lang = className?.replace("language-", "");
                  const code = String(children).replace(/\n$/, "");
                  if (!className) return <code className="text-xs">{children}</code>;
                  return <InlineCode code={code} language={lang} />;
                },
              }}
            >
              {turnText}
            </ReactMarkdown>
          </div>
        </div>
      )}
      <motion.div
        initial={{ opacity: 0, height: 0 }}
        animate={{ opacity: 1, height: "auto" }}
        exit={{ opacity: 0, height: 0 }}
        transition={{ duration: 0.25, ease: "easeInOut" }}
        className={cn(
          "relative w-full pl-8 pb-2",
          isFirst && hiddenCount === 0 && !turnText && "mt-3",
          isLast && "pb-0",
        )}
      >
        {!isLast && (
          <div className="absolute left-[11px] top-[21px] bottom-0 w-px bg-border" />
        )}
        <TimelineDot
          status={statusType}
          isCancelled={!!isCancelled}
          index={toolIndex}
        />
        <SafeToolView view={View} {...viewProps} />
      </motion.div>
    </>
  );
};

export const DefaultToolCallUI = memo(
  ToolFallbackImpl,
) as unknown as ToolCallMessagePartComponent;
DefaultToolCallUI.displayName = "ToolFallback";
