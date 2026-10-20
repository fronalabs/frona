"use client";

import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "@/components/ui/code-block";

import { detectContentType, languageFromFilename } from "@/lib/file-manager-utils";

export { languageFromFilename };

export function canPreviewFile(contentType: string, filename: string): boolean {
  const mime = detectContentType(filename, contentType);
  return mime.startsWith("text/") || [
    "application/json", "application/xml", "application/xhtml+xml",
    "application/javascript", "application/ecmascript", "application/typescript",
    "application/yaml", "application/x-yaml", "application/toml", "application/sql", "application/graphql",
  ].includes(mime);
}

/** Shared by the in-app file-preview modal and the standalone `/p/` page. */
export function FilePreviewContent({
  content,
  filename,
  contentType,
}: {
  content: string;
  filename: string;
  contentType: string;
}) {
  const lang = languageFromFilename(filename);
  const mime = detectContentType(filename, contentType);
  if (mime === "text/html" || mime === "application/xhtml+xml") {
    // Never add allow-same-origin: fetched HTML (including blobs) is untrusted.
    // The opaque origin permits scripts/CDN assets, but denies app DOM/storage.
    // Relative private assets and persistent storage are intentionally unsupported.
    return (
      <iframe
        title={filename}
        srcDoc={content}
        sandbox="allow-scripts"
        referrerPolicy="no-referrer"
        className="w-full min-h-[60vh] border-0 bg-white"
      />
    );
  }
  const isMarkdown = mime === "text/markdown";

  if (isMarkdown) {
    return (
      <div className="prose prose-sm max-w-none text-text-primary prose-headings:text-text-primary prose-strong:text-text-primary prose-a:text-accent prose-code:text-text-primary prose-code:before:content-none prose-code:after:content-none prose-blockquote:text-text-secondary prose-blockquote:border-border">
        <ReactMarkdown remarkPlugins={[remarkGfm]} components={{
          code({ className, children, ...props }) {
            const match = /language-(\w+)/.exec(className || "");
            const code = String(children).replace(/\n$/, "");
            if (match) return <CodeBlock code={code} language={match[1]} />;
            return <code className={className} {...props}>{children}</code>;
          },
        }}>
          {content}
        </ReactMarkdown>
      </div>
    );
  }

  if (lang) {
    return (
      <div className="[&_pre]:!rounded-none [&_pre]:!m-0 min-h-full">
        <CodeBlock code={content} language={lang} />
      </div>
    );
  }

  return <pre className="whitespace-pre-wrap text-sm text-text-primary font-mono">{content}</pre>;
}
