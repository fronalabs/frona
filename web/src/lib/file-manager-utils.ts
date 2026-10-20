import type { Agent, FileEntry } from "@/lib/types";
import type { IEntity } from "@svar-ui/react-filemanager";

export const MYFILES_ROOT = "/My Files";
export const WORKSPACES_ROOT = "/Workspaces";

export function toSvarEntries(
  entries: FileEntry[],
  parentPrefix: string,
): IEntity[] {
  return entries.map((e) => ({
    id: `${parentPrefix}${e.id}`,
    parent: e.parent === "/" ? parentPrefix : `${parentPrefix}${e.parent}`,
    size: e.size,
    date: e.date,
    type: e.type,
    ...(e.type === "folder" ? { lazy: true } : {}),
  }));
}

export function isWorkspacePath(path: string): boolean {
  return path.startsWith(WORKSPACES_ROOT + "/");
}

export function isMyFilesPath(path: string): boolean {
  return path === MYFILES_ROOT || path.startsWith(MYFILES_ROOT + "/");
}

export function fileBrowserAncestors(path: string): string[] {
  if (!isMyFilesPath(path) && !isWorkspacePath(path)) return [];
  const parts = path.split("/").filter(Boolean);
  const ancestors: string[] = [];
  let current = "";
  for (const part of parts) {
    current += `/${part}`;
    ancestors.push(current);
  }
  return ancestors;
}

export function userSubpath(path: string): string {
  if (path === MYFILES_ROOT) return "";
  return path.slice(MYFILES_ROOT.length + 1);
}

export function agentSubpath(path: string): string {
  const rest = path.slice(WORKSPACES_ROOT.length + 1);
  const parts = rest.split("/");
  return parts.slice(1).join("/");
}

export function resolveAgentId(path: string, agents: Agent[]): string | null {
  if (!path.startsWith(WORKSPACES_ROOT + "/")) return null;
  const rest = path.slice(WORKSPACES_ROOT.length + 1);
  const agentName = rest.split("/")[0];
  const agent = agents.find((a) => a.name === agentName);
  return agent?.id ?? null;
}

export function resolveAgentHandle(path: string, agents: Agent[]): string | null {
  if (!path.startsWith(WORKSPACES_ROOT + "/")) return null;
  const rest = path.slice(WORKSPACES_ROOT.length + 1);
  const agentName = rest.split("/")[0];
  const agent = agents.find((candidate) => candidate.name === agentName);
  return agent?.handle ?? null;
}

export function fileOperationPath(
  path: string,
  userHandle: string,
  agents: Agent[],
): string | null {
  if (isMyFilesPath(path)) {
    return `user://${userHandle}/${userSubpath(path)}`;
  }
  if (isWorkspacePath(path)) {
    const agentHandle = resolveAgentHandle(path, agents);
    return agentHandle ? `agent://${agentHandle}/${agentSubpath(path)}` : null;
  }
  return null;
}

export function getFileOwnerPath(
  fileId: string,
  userId: string,
  agents: Agent[],
): { owner: string; path: string } | null {
  if (isWorkspacePath(fileId)) {
    const agentId = resolveAgentId(fileId, agents);
    if (!agentId) return null;
    return { owner: `agent:${agentId}`, path: agentSubpath(fileId) };
  }
  return { owner: `user:${userId}`, path: userSubpath(fileId) };
}

function normalizedBasename(filename: string): string {
  return filename.split(/[\\/]/).pop()?.toLowerCase() ?? "";
}

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
  const basename = normalizedBasename(filename);
  if (basename === "dockerfile" || basename === "containerfile") return "dockerfile";
  if (basename === "makefile" || basename === "gnumakefile") return "makefile";
  const ext = basename.includes(".") ? basename.split(".").pop() : undefined;
  if (!ext) return null;
  return Object.hasOwn(EXT_TO_LANGUAGE, ext) ? EXT_TO_LANGUAGE[ext] : null;
}

const EXTENSION_MIME_MAP: Record<string, string> = {
  txt: "text/plain",
  md: "text/markdown",
  mdx: "text/markdown",
  markdown: "text/markdown",
  log: "text/plain",
  conf: "text/plain",
  cfg: "text/plain",
  ini: "text/plain",
  env: "text/plain",
  properties: "text/plain",
  html: "text/html",
  htm: "text/html",
  css: "text/css",
  js: "text/javascript",
  mjs: "text/javascript",
  ts: "text/typescript",
  tsx: "text/typescript",
  json: "application/json",
  xml: "application/xml",
  csv: "text/csv",
  pdf: "application/pdf",
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  svg: "image/svg+xml",
  webp: "image/webp",
  mp3: "audio/mpeg",
  wav: "audio/wav",
  mp4: "video/mp4",
  webm: "video/webm",
  ogg: "audio/ogg",
  oga: "audio/ogg",
  opus: "audio/ogg",
  flac: "audio/flac",
  aac: "audio/aac",
  m4a: "audio/mp4",
  m4v: "video/mp4",
  ogv: "video/ogg",
  mov: "video/quicktime",
  mkv: "video/matroska",
  avi: "video/x-msvideo",
  zip: "application/zip",
  tar: "application/x-tar",
  gz: "application/gzip",
  doc: "application/msword",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  xls: "application/vnd.ms-excel",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  ppt: "application/vnd.ms-powerpoint",
  pptx: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  yaml: "text/yaml",
  yml: "text/yaml",
  toml: "text/toml",
  rs: "text/x-rust",
  py: "text/x-python",
  rb: "text/x-ruby",
  go: "text/x-go",
  java: "text/x-java",
  sh: "text/x-shellscript",
};

const TEXT_CONFIG_NAMES = new Set([
  ".gitignore", ".gitattributes", ".gitmodules", ".dockerignore", ".editorconfig", ".npmrc", ".yarnrc",
]);

export function detectContentType(filename: string, contentType?: string | null): string {
  const mime = contentType?.split(";")[0].trim().toLowerCase();
  if (mime && mime !== "application/octet-stream") return mime;

  const basename = normalizedBasename(filename);
  if (basename === ".env" || basename.startsWith(".env.") || TEXT_CONFIG_NAMES.has(basename)) return "text/plain";
  if (basename === "dockerfile" || basename === "containerfile") return "text/x-dockerfile";
  if (basename === "makefile" || basename === "gnumakefile") return "text/x-makefile";
  const ext = basename.includes(".") ? basename.split(".").pop() : undefined;
  if (ext && Object.hasOwn(EXTENSION_MIME_MAP, ext)) return EXTENSION_MIME_MAP[ext];
  return languageFromFilename(filename) ? "text/plain" : "application/octet-stream";
}
