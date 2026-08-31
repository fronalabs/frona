"use client";

import { useState, useEffect, useCallback, useRef } from "react";
import { useAuth } from "@/lib/auth";
import { useTheme } from "@/lib/theme";
import {
  EllipsisVerticalIcon,
  ArrowUpTrayIcon,
  FolderArrowDownIcon,
  ListBulletIcon,
  Squares2X2Icon,
  ViewColumnsIcon,
} from "@heroicons/react/24/outline";
import {
  api as apiClient,
  listUserFiles,
  listAgentFiles,
  renameFile,
  copyFiles,
  moveFiles,
  createFolder,
  deleteFiles,
  uploadFile,
  searchFiles,
  presignFile,
} from "@/lib/api-client";
import type { Agent } from "@/lib/types";
import type { IEntity, IApi, IFileMenuOption } from "@svar-ui/react-filemanager";
import { Filemanager, Willow, WillowDark, getMenuOptions } from "@svar-ui/react-filemanager";
import { Locale } from "@svar-ui/react-core";
import "@svar-ui/react-filemanager/all.css";
import {
  MYFILES_ROOT,
  WORKSPACES_ROOT,
  toSvarEntries,
  isWorkspacePath,
  isMyFilesPath,
  userSubpath,
  agentSubpath,
  resolveAgentId as resolveAgentIdUtil,
  fileOperationPath,
  getFileOwnerPath as getFileOwnerPathUtil,
} from "@/lib/file-manager-utils";

export default function FilesPage() {
  const { user } = useAuth();
  const { resolved } = useTheme();

  const [agents, setAgents] = useState<Agent[]>([]);
  const [data, setData] = useState<IEntity[]>([]);
  const [loading, setLoading] = useState(true);

  const agentsRef = useRef<Agent[]>([]);
  agentsRef.current = agents;

  const fileInputRef = useRef<HTMLInputElement>(null);
  const folderInputRef = useRef<HTMLInputElement>(null);
  const fmApiRef = useRef<IApi | null>(null);

  const [showMenu, setShowMenu] = useState(false);
  const [shareUrl, setShareUrl] = useState<string | null>(null);
  const [operationError, setOperationError] = useState<string | null>(null);

  function getCurrentUploadPath(): { currentPath: string; parentSub: string } | null {
    const state = fmApiRef.current?.getState();
    const panels = state?.panels;
    const activePanel = state?.activePanel ?? 0;
    const currentPath = panels?.[activePanel]?.path ?? MYFILES_ROOT;
    if (!isMyFilesPath(currentPath)) return null;
    return { currentPath, parentSub: userSubpath(currentPath) };
  }

  function parentPath(path: string): string {
    const separator = path.lastIndexOf("/");
    return separator > 0 ? path.slice(0, separator) : path;
  }

  async function uploadWithPath(file: File, relativePath: string, createdFolders: Set<string>) {
    const parts = relativePath.split("/");
    if (parts.length > 1) {
      for (let i = 1; i < parts.length; i++) {
        const folderPath = "/" + parts.slice(0, i).join("/");
        if (!createdFolders.has(folderPath)) {
          await createFolder(folderPath);
          createdFolders.add(folderPath);
        }
      }
    }
    await uploadFile(file, relativePath);
  }

  const refreshCurrentFolder = useCallback(async () => {
    const uploadPath = getCurrentUploadPath();
    if (!uploadPath) return;
    const { currentPath } = uploadPath;
    if (!fmApiRef.current) return;
    fmApiRef.current.exec("provide-data", { id: currentPath, data: [] as IEntity[] });
    const sub = userSubpath(currentPath);
    const entries = await listUserFiles(sub || undefined);
    fmApiRef.current.exec("provide-data", { id: currentPath, data: toSvarEntries(entries, MYFILES_ROOT) });
  }, []);

  const handleFileInputChange = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files;
    if (!files || files.length === 0) return;
    const uploadPath = getCurrentUploadPath();
    if (!uploadPath) {
      setOperationError("Upload files to My Files, then copy or move them into an agent workspace");
      e.target.value = "";
      return;
    }
    const { parentSub } = uploadPath;
    setOperationError(null);
    const createdFolders = new Set<string>();
    for (const file of Array.from(files)) {
      const filePath = file.webkitRelativePath || file.name;
      const relativePath = parentSub ? `${parentSub}/${filePath}` : filePath;
      try {
        await uploadWithPath(file, relativePath, createdFolders);
      } catch (error) {
        setOperationError(error instanceof Error ? error.message : "Upload failed");
      }
    }
    await refreshCurrentFolder();
    e.target.value = "";
  };

  const handleDrop = useCallback(async (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();

    const items = e.dataTransfer.items;
    if (!items || items.length === 0) return;

    const entries: FileSystemEntry[] = [];
    let hasDirectory = false;
    for (const item of Array.from(items)) {
      const entry = item.webkitGetAsEntry?.();
      if (entry) {
        entries.push(entry);
        if (entry.isDirectory) hasDirectory = true;
      }
    }

    if (!hasDirectory) return;

    async function readEntry(entry: FileSystemEntry, path: string): Promise<{ file: File; path: string }[]> {
      if (entry.isFile) {
        const file = await new Promise<File>((resolve) =>
          (entry as FileSystemFileEntry).file(resolve),
        );
        return [{ file, path: path + entry.name }];
      }
      if (entry.isDirectory) {
        const dirReader = (entry as FileSystemDirectoryEntry).createReader();
        const children = await new Promise<FileSystemEntry[]>((resolve) =>
          dirReader.readEntries(resolve),
        );
        const results: { file: File; path: string }[] = [];
        for (const child of children) {
          results.push(...await readEntry(child, path + entry.name + "/"));
        }
        return results;
      }
      return [];
    }

    const uploadPath = getCurrentUploadPath();
    if (!uploadPath) {
      setOperationError("Upload files to My Files, then copy or move them into an agent workspace");
      return;
    }
    const { parentSub } = uploadPath;
    setOperationError(null);
    const createdFolders = new Set<string>();
    const allFiles: { file: File; path: string }[] = [];

    for (const entry of entries) {
      allFiles.push(...await readEntry(entry, ""));
    }

    for (const { file, path } of allFiles) {
      const relativePath = parentSub ? `${parentSub}/${path}` : path;
      try {
        await uploadWithPath(file, relativePath, createdFolders);
      } catch (error) {
        setOperationError(error instanceof Error ? error.message : "Upload failed");
      }
    }

    await refreshCurrentFolder();
  }, [refreshCurrentFolder]);

  useEffect(() => {
    apiClient.get<Agent[]>("/api/agents").then(setAgents).catch((error) => {
      setOperationError(error instanceof Error ? error.message : "Unable to load agents");
    });
  }, []);

  const buildRootData = useCallback(
    async (agentList: Agent[]) => {
      setLoading(true);
      try {
        const userFiles = await listUserFiles();
        const rootEntries: IEntity[] = [
          {
            id: MYFILES_ROOT,
            type: "folder",
            size: 0,
            date: new Date(),
            lazy: false,
          },
          {
            id: WORKSPACES_ROOT,
            type: "folder",
            size: 0,
            date: new Date(),
            lazy: false,
          },
          ...toSvarEntries(userFiles, MYFILES_ROOT),
          ...agentList.map((a) => ({
            id: `${WORKSPACES_ROOT}/${a.name}`,
            type: "folder" as const,
            size: 0,
            date: new Date(),
            lazy: true,
            _agentId: a.id,
          })),
        ];
        setData(rootEntries);
      } catch (error) {
        setOperationError(error instanceof Error ? error.message : "Unable to load files");
        setData([]);
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  useEffect(() => {
    if (agents.length > 0) {
      buildRootData(agents);
    } else {
      buildRootData([]);
    }
  }, [agents, buildRootData]);

  function resolveAgentId(path: string): string | null {
    return resolveAgentIdUtil(path, agentsRef.current);
  }

  const getFileOwnerPath = useCallback((fileId: string): { owner: string; path: string } | null => {
    if (!user) return null;
    return getFileOwnerPathUtil(fileId, user.id, agentsRef.current);
  }, [user]);

  const handleInit = useCallback(
    (fmApi: IApi) => {
      fmApiRef.current = fmApi;

      fmApi.exec("set-path", { id: MYFILES_ROOT });

      fmApi.on("filter-files", (ev) => {
        if (!ev.text) return;

        searchFiles(ev.text).then((results) => {
          const byParent = new Map<string, IEntity[]>();

          for (const result of results) {
            let treeId: string;
            const relPath = result.path.replace(/^\//, "");
            if (result.source === "user") {
              treeId = relPath ? `${MYFILES_ROOT}/${relPath}` : MYFILES_ROOT;
            } else {
              const agent = agentsRef.current.find((a) => a.id === result.source);
              if (!agent) continue;
              treeId = relPath
                ? `${WORKSPACES_ROOT}/${agent.name}/${relPath}`
                : `${WORKSPACES_ROOT}/${agent.name}`;
            }

            if (fmApi.getFile(treeId)) continue;

            const lastSlash = treeId.lastIndexOf("/");
            const parentId = lastSlash <= 0 ? "/" : treeId.substring(0, lastSlash);

            const parts = treeId.split("/").filter(Boolean);
            for (let i = 1; i < parts.length - 1; i++) {
              const folderPath = "/" + parts.slice(0, i + 1).join("/");
              if (!fmApi.getFile(folderPath)) {
                const folderParent = i === 0 ? "/" : "/" + parts.slice(0, i).join("/");
                const group = byParent.get(folderParent) || [];
                group.push({
                  id: folderPath,
                  type: "folder",
                  size: 0,
                  date: new Date(),
                  lazy: true,
                });
                byParent.set(folderParent, group);
              }
            }

            const group = byParent.get(parentId) || [];
            group.push({
              id: treeId,
              type: result.type,
              size: result.size,
              date: result.date,
            });
            byParent.set(parentId, group);
          }

          for (const [parentId, entries] of byParent) {
            fmApi.exec("provide-data", { id: parentId, data: entries });
          }
        }).catch((error) => {
          setOperationError(error instanceof Error ? error.message : "Search failed");
        });
      });

      fmApi.intercept("open-file", (ev) => {
        const id = ev.id as string;
        const file = fmApi.getFile(id);
        if (!file || file.type === "folder") return;

        const parentId = file.parent as string;
        fmApi.exec("set-mode", { mode: "table" });
        fmApi.exec("set-path", { id: parentId, selected: [id] });
        return false;
      });

      fmApi.on("set-path", (ev) => {
        const id = ev.id as string;
        const parts = id.split("/").filter(Boolean);
        let path = "";
        for (const part of parts) {
          path += "/" + part;
          const node = fmApi.getFile(path);
          if (node && node.type === "folder" && !node.open) {
            fmApi.exec("open-tree-folder", { id: path, mode: true });
          }
        }
      });

      fmApi.on("rename-file", async (ev) => {
        if (!user) return;
        const filePath = fileOperationPath(ev.id, user.handle, agentsRef.current);
        if (!filePath) return;
        setOperationError(null);
        try {
          await renameFile(filePath, ev.name);
        } catch (error) {
          setOperationError(error instanceof Error ? error.message : "Rename failed");
        } finally {
          await fmApi.exec("request-data", { id: parentPath(ev.id) });
        }
      });

      fmApi.on("delete-files", async (ev) => {
        if (!user) return;
        const paths = ev.ids.map((id: string) =>
          fileOperationPath(id, user.handle, agentsRef.current));
        if (paths.some((path) => path === null)) return;
        setOperationError(null);
        try {
          await deleteFiles(paths as string[]);
        } catch (error) {
          setOperationError(error instanceof Error ? error.message : "Delete failed");
        } finally {
          const parents = new Set(
            ev.ids.map((id: string) => {
              return parentPath(id);
            }),
          );
          for (const parent of parents) {
            await fmApi.exec("request-data", { id: parent });
          }
        }
      });

      fmApi.on("copy-files", async (ev) => {
        if (!user) return;
        const sources = ev.ids.map((id: string) =>
          fileOperationPath(id, user.handle, agentsRef.current));
        const dest = fileOperationPath(ev.target, user.handle, agentsRef.current);
        if (!dest || sources.some((source) => source === null)) return;
        setOperationError(null);
        try {
          await copyFiles(sources as string[], dest);
        } catch (error) {
          setOperationError(error instanceof Error ? error.message : "Copy failed");
        } finally {
          await fmApi.exec("request-data", { id: ev.target });
        }
      });

      fmApi.on("move-files", async (ev) => {
        if (!user) return;
        const sources = ev.ids.map((id: string) =>
          fileOperationPath(id, user.handle, agentsRef.current));
        const dest = fileOperationPath(ev.target, user.handle, agentsRef.current);
        if (!dest || sources.some((source) => source === null)) return;
        setOperationError(null);
        try {
          await moveFiles(sources as string[], dest);
        } catch (error) {
          setOperationError(error instanceof Error ? error.message : "Move failed");
        } finally {
          const folders = new Set([ev.target, ...ev.ids.map((id: string) => parentPath(id))]);
          for (const folder of folders) {
            await fmApi.exec("request-data", { id: folder });
          }
        }
      });

      fmApi.on("create-file", async (ev) => {
        if (ev.file.type === "folder") {
          if (!user) return;
          const path = fileOperationPath(
            `${ev.parent}/${ev.file.name}`,
            user.handle,
            agentsRef.current,
          );
          if (!path) return;
          setOperationError(null);
          try {
            await createFolder(path);
          } catch (error) {
            setOperationError(error instanceof Error ? error.message : "Create folder failed");
          } finally {
            await fmApi.exec("request-data", { id: ev.parent });
          }
        } else {
          if (!isMyFilesPath(ev.parent)) {
            setOperationError("Upload files to My Files, then copy or move them into an agent workspace");
            await fmApi.exec("request-data", { id: ev.parent });
            return;
          }
          const parentSub = userSubpath(ev.parent);
          const file = ev.file.file
            ? (ev.file.file as File)
            : new File([], ev.file.name);
          const relativePath = parentSub
            ? `${parentSub}/${ev.file.name}`
            : ev.file.name;
          try {
            await uploadFile(file, relativePath);
          } catch (error) {
            setOperationError(error instanceof Error ? error.message : "Upload failed");
          } finally {
            await fmApi.exec("request-data", { id: ev.parent });
          }
        }
      });

      fmApi.on("download-file", async (ev) => {
        const info = getFileOwnerPath(ev.id);
        if (!info) return;
        try {
          const url = await presignFile(info.owner, info.path);
          const res = await fetch(url);
          if (!res.ok) throw new Error(`Download failed: ${res.statusText}`);
          const blob = await res.blob();
          const blobUrl = URL.createObjectURL(blob);
          const a = document.createElement("a");
          a.href = blobUrl;
          a.download = ev.id.split("/").pop() || "download";
          a.style.display = "none";
          document.body.appendChild(a);
          a.click();
          document.body.removeChild(a);
          URL.revokeObjectURL(blobUrl);
        } catch (error) {
          setOperationError(error instanceof Error ? error.message : "Download failed");
        }
      });

      fmApi.on("request-data", async (ev) => {
        try {
          let svarEntries: IEntity[];

          if (ev.id === WORKSPACES_ROOT) {
            svarEntries = agentsRef.current.map((a) => ({
              id: `${WORKSPACES_ROOT}/${a.name}`,
              type: "folder" as const,
              size: 0,
              date: new Date(),
              lazy: true,
            }));
          } else if (isWorkspacePath(ev.id)) {
            const agentId = resolveAgentId(ev.id);
            if (!agentId) {
              fmApi.exec("provide-data", { id: ev.id, data: [] });
              return;
            }
            const agentName = ev.id.slice(WORKSPACES_ROOT.length + 1).split("/")[0];
            const agentRoot = `${WORKSPACES_ROOT}/${agentName}`;
            const sub = agentSubpath(ev.id);
            const entries = await listAgentFiles(agentId, sub || undefined);
            svarEntries = toSvarEntries(entries, agentRoot);
          } else if (isMyFilesPath(ev.id)) {
            const sub = userSubpath(ev.id);
            const entries = await listUserFiles(sub || undefined);
            svarEntries = toSvarEntries(entries, MYFILES_ROOT);
          } else {
            svarEntries = [];
          }

          fmApi.exec("provide-data", {
            id: ev.id,
            data: svarEntries,
          });
        } catch (error) {
          setOperationError(error instanceof Error ? error.message : "Unable to load folder");
          fmApi.exec("provide-data", { id: ev.id, data: [] });
        }
      });
    },
    [user, getFileOwnerPath],
  );

  const ThemeWrapper = resolved === "dark" ? WillowDark : Willow;

  return (
    <div className="flex h-full min-h-0 bg-surface">
      <div className="flex-1 min-w-0 overflow-hidden">
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={handleFileInputChange}
        />
        <input
          ref={folderInputRef}
          type="file"
          className="hidden"
          onChange={handleFileInputChange}
          {...{ webkitdirectory: "", directory: "" } as React.InputHTMLAttributes<HTMLInputElement>}
        />
        {loading && data.length === 0 ? (
          <div className="flex items-center justify-center h-full">
            <p className="text-sm text-text-tertiary">Loading files...</p>
          </div>
        ) : (
          <div
            className="h-full filemanager-container relative"
            onDrop={handleDrop}
            onDragOver={(e) => e.preventDefault()}
          >
            {operationError && (
              <div
                role="alert"
                className="absolute left-4 right-4 top-2 z-20 rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-700 dark:text-red-300"
              >
                {operationError}
              </div>
            )}
            <div className="absolute top-2 right-4 z-10">
              {showMenu && (
                <>
                  <div className="fixed inset-0" onClick={() => setShowMenu(false)} />
                  <div className="absolute top-10 right-0 bg-surface-secondary border border-border rounded-lg shadow-lg py-1 min-w-[160px]">
                    <div className="px-3 py-1.5 text-[11px] font-semibold uppercase tracking-wider text-text-tertiary">View</div>
                    <button
                      onClick={() => { fmApiRef.current?.exec("set-mode", { mode: "table" }); setShowMenu(false); }}
                      className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-primary hover:bg-surface-tertiary text-left"
                    >
                      <ListBulletIcon className="h-4 w-4 text-text-secondary" />
                      List
                    </button>
                    <button
                      onClick={() => { fmApiRef.current?.exec("set-mode", { mode: "cards" }); setShowMenu(false); }}
                      className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-primary hover:bg-surface-tertiary text-left"
                    >
                      <Squares2X2Icon className="h-4 w-4 text-text-secondary" />
                      Grid
                    </button>
                    <button
                      onClick={() => { fmApiRef.current?.exec("set-mode", { mode: "panels" }); setShowMenu(false); }}
                      className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-primary hover:bg-surface-tertiary text-left"
                    >
                      <ViewColumnsIcon className="h-4 w-4 text-text-secondary" />
                      Columns
                    </button>
                    <div className="border-t border-border my-1" />
                    <button
                      onClick={() => { fileInputRef.current?.click(); setShowMenu(false); }}
                      className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-primary hover:bg-surface-tertiary text-left"
                    >
                      <ArrowUpTrayIcon className="h-4 w-4 text-text-secondary" />
                      Upload Files
                    </button>
                    <button
                      onClick={() => { folderInputRef.current?.click(); setShowMenu(false); }}
                      className="w-full flex items-center gap-2 px-4 py-2 text-sm text-text-primary hover:bg-surface-tertiary text-left"
                    >
                      <FolderArrowDownIcon className="h-4 w-4 text-text-secondary" />
                      Upload Folder
                    </button>
                  </div>
                </>
              )}
              <button
                onClick={() => setShowMenu((v) => !v)}
                className="flex items-center gap-2 px-4 py-2 text-sm font-medium rounded-lg bg-accent text-surface hover:bg-accent-hover transition shadow"
                title="Options"
              >
                <EllipsisVerticalIcon className="h-5 w-5" />
                Options
              </button>
            </div>
            <ThemeWrapper>
              <Locale words={{ filemanager: { "My files": " " } }}>
              <Filemanager
                data={data}
                init={handleInit}
                mode="table"
                menuOptions={(mode, item) => {
                  const opts = getMenuOptions(mode) as IFileMenuOption[];
                  if (mode === "add") {
                    for (const opt of opts) {
                      if (opt.id === "add-file") opt.icon = "wxi-content-paste";
                      if (opt.id === "add-folder") opt.icon = "wxi-folder";
                      if (opt.id === "upload") opt.icon = "wxi-arrow-up";
                    }
                    opts.push({
                      icon: "wxi-arrow-up", text: "Upload Folder", hotkey: "", id: "upload-folder",
                      handler: () => { folderInputRef.current?.click(); },
                    });
                  }
                  if (mode === "file" && item) {
                    const downloadIdx = opts.findIndex((o) => o.id === "download");
                    const fileId = item.id;
                    const extra = [
                      {
                        icon: "wxi-eye", text: "Open", hotkey: "", id: "open-file-url",
                        handler: () => {
                          const info = getFileOwnerPath(fileId);
                          if (info) {
                            presignFile(info.owner, info.path)
                              .then((url) => window.open(url, "_blank"))
                              .catch((error) => setOperationError(error instanceof Error ? error.message : "Open failed"));
                          }
                        },
                      },
                      {
                        icon: "wxi-content-copy", text: "Share", hotkey: "", id: "share-file-url",
                        handler: () => {
                          const info = getFileOwnerPath(fileId);
                          if (info) {
                            presignFile(info.owner, info.path)
                              .then((url) => setShareUrl(url))
                              .catch((error) => setOperationError(error instanceof Error ? error.message : "Share failed"));
                          }
                        },
                      },
                    ] as IFileMenuOption[];
                    opts.splice(downloadIdx >= 0 ? downloadIdx + 1 : 0, 0, ...extra);
                  }
                  return opts;
                }}
                {...{
                  uploadURL: async (fileInfo: { id: string; file: File; name: string }) => {
                    const uploadPath = getCurrentUploadPath();
                    if (!uploadPath) {
                      setOperationError("Upload files to My Files, then copy or move them into an agent workspace");
                      return { id: fileInfo.id, status: "error" };
                    }
                    const { parentSub } = uploadPath;
                    const relativePath = parentSub
                      ? `${parentSub}/${fileInfo.name}`
                      : fileInfo.name;
                    try {
                      const result = await uploadFile(fileInfo.file, relativePath);
                      return { id: fileInfo.id, status: "server", ...result };
                    } catch (error) {
                      setOperationError(error instanceof Error ? error.message : "Upload failed");
                      return { id: fileInfo.id, status: "error" };
                    }
                  },
                }}
              />
              </Locale>
            </ThemeWrapper>
          </div>
        )}
      </div>
      {shareUrl && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/30" onClick={() => setShareUrl(null)}>
          <div className="bg-surface border border-border rounded-lg p-6 shadow-lg w-full max-w-md" onClick={(e) => e.stopPropagation()}>
            <h3 className="text-sm font-medium text-text-primary mb-3">Share Link</h3>
            <div className="flex gap-2">
              <input
                type="text"
                readOnly
                value={shareUrl}
                className="flex-1 rounded-md border border-border bg-surface-secondary px-3 py-2 text-sm text-text-primary"
                onFocus={(e) => e.target.select()}
              />
              <button
                onClick={() => {
                  navigator.clipboard.writeText(shareUrl);
                }}
                className="px-3 py-2 text-sm font-medium rounded-md bg-accent text-surface hover:bg-accent-hover transition"
              >
                Copy
              </button>
            </div>
            <div className="mt-4 flex justify-center">
              <button
                onClick={() => setShareUrl(null)}
                className="px-4 py-2 text-sm font-medium rounded-md border border-border text-text-primary hover:bg-surface-tertiary transition"
              >
                Close
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
