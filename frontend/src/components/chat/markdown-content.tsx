"use client";

import { useState, useCallback } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import { ClipboardIcon, CheckIcon } from "@heroicons/react/24/outline";
import type { Components } from "react-markdown";
import type { Element, Text } from "hast";

interface MarkdownContentProps {
  content: string;
}

function CopyButton({ code }: { code: string }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  }, [code]);

  return (
    <button
      onClick={handleCopy}
      className="absolute right-2 top-2 rounded p-1 text-text-tertiary opacity-0 transition-opacity hover:text-text-secondary group-hover:opacity-100"
      aria-label="Copy code"
    >
      {copied ? (
        <CheckIcon className="h-4 w-4 text-success" />
      ) : (
        <ClipboardIcon className="h-4 w-4" />
      )}
    </button>
  );
}

function extractCode(node: Element | undefined): { code: string; language: string } | null {
  if (!node) return null;
  const codeEl = node.children?.find(
    (c): c is Element => c.type === "element" && c.tagName === "code",
  );
  if (!codeEl) return null;

  const classNames = (codeEl.properties?.className as string[]) || [];
  const match = /language-(\w+)/.exec(classNames.join(" "));
  const code = codeEl.children
    .filter((c): c is Text => c.type === "text")
    .map((c) => c.value)
    .join("")
    .replace(/\n$/, "");

  return { code, language: match?.[1] || "text" };
}

const components: Components = {
  pre({ node, children }) {
    const extracted = extractCode(node as Element | undefined);
    if (!extracted) return <pre>{children}</pre>;

    return (
      <div className="group relative">
        <CopyButton code={extracted.code} />
        <SyntaxHighlighter
          style={oneDark}
          language={extracted.language}
          PreTag="div"
          customStyle={{
            margin: 0,
            borderRadius: "0.375rem",
            fontSize: "0.8125rem",
          }}
        >
          {extracted.code}
        </SyntaxHighlighter>
      </div>
    );
  },
};

export function MarkdownContent({ content }: MarkdownContentProps) {
  return (
    <div className="prose prose-sm max-w-none leading-normal prose-p:my-1.5 prose-headings:my-2 prose-ul:my-1.5 prose-ol:my-1.5 prose-li:my-0 prose-pre:my-2 prose-blockquote:my-1.5 prose-hr:my-2 text-[var(--text-primary)] prose-headings:text-[var(--text-primary)] prose-strong:text-[var(--text-primary)] prose-a:text-[var(--accent)] hover:prose-a:text-[var(--accent-hover)] prose-code:text-[var(--text-primary)] prose-code:before:content-none prose-code:after:content-none prose-blockquote:text-[var(--text-secondary)] prose-blockquote:border-[var(--border)] prose-hr:border-[var(--border)] prose-th:border-[var(--border)] prose-td:border-[var(--border)]">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {content}
      </ReactMarkdown>
    </div>
  );
}
