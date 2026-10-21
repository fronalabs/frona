"use client";

import { MarkdownTextPrimitive } from "@assistant-ui/react-markdown";
import { useMessagePartText } from "@assistant-ui/react";
import type { SyntaxHighlighterProps } from "@assistant-ui/react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "@/components/ui/code-block";
import { MarkdownErrorBoundary } from "./markdown-error-boundary";

function SyntaxHighlighter({ code, language }: SyntaxHighlighterProps) {
  return <CodeBlock code={code} language={language} />;
}

export function MarkdownText({ smooth }: { smooth?: boolean; [key: string]: unknown }) {
  const { text } = useMessagePartText();
  return (
    <MarkdownErrorBoundary text={text}>
      <MarkdownTextPrimitive
        className="prose prose-base max-w-none leading-normal [&>*:first-child]:mt-0 [&>*:last-child]:mb-0 prose-p:my-1.5 prose-headings:my-2 prose-ul:my-1.5 prose-ol:my-1.5 prose-li:my-0 prose-pre:my-2 prose-blockquote:my-1.5 prose-hr:my-2 text-[var(--text-primary)] prose-headings:text-[var(--text-primary)] prose-strong:text-[var(--text-primary)] prose-a:text-[var(--accent)] hover:prose-a:text-[var(--accent-hover)] prose-code:text-[var(--text-primary)] prose-code:before:content-none prose-code:after:content-none prose-blockquote:text-[var(--text-secondary)] prose-blockquote:border-[var(--border)] prose-hr:border-[var(--border)] prose-th:border-[var(--border)] prose-td:border-[var(--border)]"
        remarkPlugins={[[remarkGfm, { singleTilde: false }]]}
        smooth={smooth}
        components={{
          SyntaxHighlighter,
        }}
      />
    </MarkdownErrorBoundary>
  );
}
