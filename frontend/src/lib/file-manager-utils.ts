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

const EXTENSION_MIME_MAP: Record<string, string> = {
  txt: "text/plain",
  md: "text/markdown",
  html: "text/html",
  htm: "text/html",
  css: "text/css",
  js: "application/javascript",
  ts: "application/typescript",
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
  zip: "application/zip",
  tar: "application/x-tar",
  gz: "application/gzip",
  doc: "application/msword",
  docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  xls: "application/vnd.ms-excel",
  xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  ppt: "application/vnd.ms-powerpoint",
  pptx: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  yaml: "application/x-yaml",
  yml: "application/x-yaml",
  toml: "application/toml",
  rs: "text/x-rust",
  py: "text/x-python",
  rb: "text/x-ruby",
  go: "text/x-go",
  java: "text/x-java",
  sh: "text/x-shellscript",
};

export function detectContentType(filename: string): string {
  const ext = filename.split(".").pop()?.toLowerCase();
  if (ext && ext in EXTENSION_MIME_MAP) return EXTENSION_MIME_MAP[ext];
  return "application/octet-stream";
}
