"use client";

import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "@/components/ui/code-block";

const EXT_TO_LANGUAGE: Record<string, string> = {
  js: "javascript", jsx: "jsx", ts: "typescript", tsx: "tsx",
  py: "python", rb: "ruby", rs: "rust", go: "go", java: "java",
  c: "c", cpp: "cpp", h: "c", hpp: "cpp", cs: "csharp",
  swift: "swift", kt: "kotlin", scala: "scala", php: "php",
  sh: "bash", bash: "bash", zsh: "bash", fish: "fish",
  html: "html", htm: "html", css: "css", scss: "scss", less: "less",
  json: "json", yaml: "yaml", yml: "yaml", toml: "toml", xml: "xml",
  sql: "sql", graphql: "graphql", gql: "graphql",
  md: "markdown", mdx: "mdx", tex: "latex",
  dockerfile: "dockerfile", makefile: "makefile",
  tf: "hcl", hcl: "hcl", nix: "nix",
  lua: "lua", r: "r", dart: "dart", zig: "zig", v: "v",
  svelte: "svelte", vue: "vue", astro: "astro",
};

export function languageFromFilename(filename: string): string | null {
  const ext = filename.split(".").pop()?.toLowerCase();
  if (!ext) return null;
  return EXT_TO_LANGUAGE[ext] ?? null;
}

export function canPreviewFile(contentType: string, filename: string): boolean {
  if (contentType.startsWith("text/")) return true;
  if (contentType === "application/json") return true;
  if (contentType === "application/xml" || contentType === "application/xhtml+xml") return true;
  if (languageFromFilename(filename)) return true;
  return false;
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
  const mime = contentType.split(";")[0].trim().toLowerCase();
  if (mime === "text/html" || mime === "application/xhtml+xml" || /\.html?$/i.test(filename)) {
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
  const isMarkdown =
    contentType === "text/markdown" ||
    filename.endsWith(".md") ||
    filename.endsWith(".mdx");

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
