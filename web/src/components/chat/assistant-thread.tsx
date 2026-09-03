"use client";

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ThreadPrimitive } from "@assistant-ui/react";
import { FronaUserMessage } from "./frona-user-message";
import { FronaAssistantMessage } from "./frona-assistant-message";
import { FronaComposer } from "./frona-composer";
import { ExternalToolDrawer, CollapsedToolTab, useToolWizard } from "./external-tool-drawer";
import { WizardAnswersContext } from "@/lib/wizard-answers-context";
import { usePendingTools } from "@/lib/pending-tools-context";
import { useChatPagination } from "@/lib/chat-pagination-context";
import {
  ChatActivityDrawer,
  ChatActivityTab,
  useChatExecutions,
} from "./chat-activity-drawer";

export function AssistantThread({ chatId }: { chatId?: string }) {
  const wizard = useToolWizard();
  const chatExecutions = useChatExecutions(chatId);
  const [activityOpen, setActivityOpen] = useState(false);
  const wizardSetCollapsed = wizard.setCollapsed;
  const lastScrollTop = useRef(0);
  const updating = useRef(false);
  const { hasMore, loadingMore, loadOlder } = useChatPagination();
  const viewportElRef = useRef<HTMLElement | null>(null);
  // Anchor on a real message DOM node so scroll position survives the
  // prepend regardless of async height changes (markdown, images, etc).
  const anchorRef = useRef<{ id: string; offset: number } | null>(null);

  const setCollapsed = useCallback(
    (v: boolean | ((prev: boolean) => boolean)) => {
      updating.current = true;
      wizardSetCollapsed(v);
      requestAnimationFrame(() => {
        updating.current = false;
      });
    },
    [wizardSetCollapsed],
  );

  const handleScroll = useCallback(
    (e: React.UIEvent<HTMLDivElement>) => {
      const el = e.currentTarget;
      viewportElRef.current = el;
      if (updating.current) return;

      const { scrollTop, scrollHeight, clientHeight } = el;
      const delta = scrollTop - lastScrollTop.current;
      lastScrollTop.current = scrollTop;

      const isNearBottom = scrollHeight - scrollTop - clientHeight < 80;

      if (delta < -10 && !isNearBottom && wizard.submitted) {
        setCollapsed(true);
      } else if (isNearBottom) {
        setCollapsed(false);
      }

      if (scrollTop < 200 && hasMore && !loadingMore && !anchorRef.current) {
        const firstMsg = el.querySelector<HTMLElement>("[data-message-id]");
        if (firstMsg) {
          const vTop = el.getBoundingClientRect().top;
          anchorRef.current = {
            id: firstMsg.dataset.messageId!,
            offset: firstMsg.getBoundingClientRect().top - vTop,
          };
        }
        loadOlder();
      }
    },
    [setCollapsed, wizard.submitted, hasMore, loadingMore, loadOlder],
  );

  useLayoutEffect(() => {
    if (loadingMore) return;
    const el = viewportElRef.current;
    const anchor = anchorRef.current;
    if (!el || !anchor) return;
    const target = el.querySelector<HTMLElement>(
      `[data-message-id="${CSS.escape(anchor.id)}"]`,
    );
    if (target) {
      const vTop = el.getBoundingClientRect().top;
      const currentOffset = target.getBoundingClientRect().top - vTop;
      el.scrollTop += currentOffset - anchor.offset;
    }
    anchorRef.current = null;
  }, [loadingMore]);

  const safeWizard = useMemo(
    () => ({ ...wizard, setCollapsed }),
    [wizard, setCollapsed],
  );

  const pendingTools = usePendingTools();
  const pendingToolIds = pendingTools.map((tool) => tool.id).join(",");
  const hasPendingTools = pendingTools.length > 0 && !wizard.submitted;
  const hasChatActivity = chatExecutions.length > 0;

  useEffect(() => {
    if (!hasChatActivity) setActivityOpen(false);
  }, [hasChatActivity]);

  useEffect(() => {
    if (!hasPendingTools) return;
    setActivityOpen(false);
    setCollapsed(false);
  }, [hasPendingTools, pendingToolIds, setCollapsed]);

  const expandActivity = useCallback(() => {
    setCollapsed(true);
    setActivityOpen(true);
  }, [setCollapsed]);

  const expandQuestion = useCallback(() => {
    setActivityOpen(false);
  }, []);

  const hasCollapsedAccessory =
    (hasPendingTools && safeWizard.collapsed) ||
    (hasChatActivity && !activityOpen);
  const hasComposerAccessory = hasPendingTools || hasChatActivity;

  return (
    <WizardAnswersContext value={wizard.answers}>
    <ThreadPrimitive.Root className="flex flex-1 flex-col min-h-0">
      <ThreadPrimitive.Viewport className="flex-1 overflow-y-auto min-h-0" onScroll={handleScroll}>
        <ThreadPrimitive.If empty>
          <div />
        </ThreadPrimitive.If>
        <ThreadPrimitive.If empty={false}>
          <div className="mx-auto w-full max-w-3xl px-3 md:px-6 py-4 space-y-3">
            <ThreadPrimitive.Messages
            components={{
              UserMessage: FronaUserMessage,
              AssistantMessage: FronaAssistantMessage,
            }}
          />
          </div>
        </ThreadPrimitive.If>
      </ThreadPrimitive.Viewport>
      <ThreadPrimitive.ViewportFooter className="sticky bottom-0">
        <ThreadPrimitive.ScrollToBottom asChild>
          <button className={`absolute left-1/2 -translate-x-1/2 z-20 rounded-full border border-border bg-surface px-3 py-1 text-xs text-text-secondary shadow-sm hover:bg-surface-secondary transition disabled:hidden ${
            hasCollapsedAccessory ? "-top-16" : "-top-10"
          }`}>
            Scroll to bottom
          </button>
        </ThreadPrimitive.ScrollToBottom>
        <div className="relative mx-auto w-full max-w-3xl px-3 md:px-6 pb-4">
          <div className="relative z-0 flex items-end justify-center gap-1 px-3 md:px-6">
            <CollapsedToolTab wizard={safeWizard} onExpand={expandQuestion} />
            {!activityOpen && (
              <ChatActivityTab executions={chatExecutions} onExpand={expandActivity} />
            )}
          </div>
          <div className={`relative z-10 rounded-2xl transition-colors ${
            hasComposerAccessory
              ? "border border-border bg-surface-secondary focus-within:border-accent"
              : "has-[.tool-drawer]:border has-[.tool-drawer]:border-border has-[.tool-drawer]:bg-surface-secondary has-[.tool-drawer]:focus-within:border-accent focus-within:border-accent"
          }`}>
            {activityOpen && (
              <ChatActivityDrawer
                executions={chatExecutions}
                onCollapse={() => setActivityOpen(false)}
              />
            )}
            <ExternalToolDrawer wizard={safeWizard} />
            <FronaComposer wizard={safeWizard} />
          </div>
        </div>
      </ThreadPrimitive.ViewportFooter>
    </ThreadPrimitive.Root>
    </WizardAnswersContext>
  );
}
