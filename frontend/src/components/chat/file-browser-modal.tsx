"use client";

import { useState, useEffect, useCallback, useRef } from "react";
import { useAuth } from "@/lib/auth";
import { useTheme } from "@/lib/theme";
import {
  api as apiClient,
  listUserFiles,
  listAgentFiles,
} from "@/lib/api-client";
import { FolderOpenIcon } from "@heroicons/react/24/outline";
import type { Agent, Attachment } from "@/lib/types";
import type { IEntity, IApi } from "@svar-ui/react-filemanager";
import { Filemanager, Willow, WillowDark } from "@svar-ui/react-filemanager";
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
  resolveAgentId,
  getFileOwnerPath,
  detectContentType,
} from "@/lib/file-manager-utils";

interface FileBrowserModalProps {
  open: boolean;
  onClose: () => void;
  onSelect: (attachments: Attachment[]) => void;
}

export function FileBrowserModal({ open, onClose, onSelect }: FileBrowserModalProps) {
  const { user } = useAuth();
  const { resolved } = useTheme();

  const [agents, setAgents] = useState<Agent[]>([]);
  const [data, setData] = useState<IEntity[]>([]);
  const [loading, setLoading] = useState(true);
  const [selectedCount, setSelectedCount] = useState(0);

  const agentsRef = useRef<Agent[]>([]);
  agentsRef.current = agents;

  const fmApiRef = useRef<IApi | null>(null);

  useEffect(() => {
    if (!open) return;
    apiClient.get<Agent[]>("/api/agents").then(setAgents).catch(() => {});
  }, [open]);

  const buildRootData = useCallback(async (agentList: Agent[]) => {
    setLoading(true);
    try {
      const userFiles = await listUserFiles();
      const rootEntries: IEntity[] = [
        { id: MYFILES_ROOT, type: "folder", size: 0, date: new Date(), lazy: false },
        { id: WORKSPACES_ROOT, type: "folder", size: 0, date: new Date(), lazy: false },
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
    } catch {
      setData([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!open) return;
    buildRootData(agents);
  }, [agents, open, buildRootData]);

  const handleInit = useCallback(
    (fmApi: IApi) => {
      fmApiRef.current = fmApi;
      fmApi.exec("set-path", { id: MYFILES_ROOT });

      fmApi.on("select-file", () => {
        const state = fmApi.getState();
        const activePanel = state?.activePanel ?? 0;
        const selected = state?.panels?.[activePanel]?.selected ?? [];
        setSelectedCount(selected.length);
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
            const agentId = resolveAgentId(ev.id, agentsRef.current);
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

          fmApi.exec("provide-data", { id: ev.id, data: svarEntries });
        } catch {
          fmApi.exec("provide-data", { id: ev.id, data: [] });
        }
      });
    },
    [user],
  );

  const handleAttach = () => {
    const api = fmApiRef.current;
    if (!api || !user) return;

    const state = api.getState();
    const activePanel = state?.activePanel ?? 0;
    const selectedIds: string[] = state?.panels?.[activePanel]?.selected ?? [];
    if (selectedIds.length === 0) return;

    const attachments: Attachment[] = [];
    for (const id of selectedIds) {
      const file = api.getFile(id);
      if (!file || file.type === "folder") continue;

      const info = getFileOwnerPath(id, user.id, agentsRef.current);
      if (!info) continue;

      const filename = id.split("/").pop() || "file";
      attachments.push({
        filename,
        content_type: detectContentType(filename),
        size_bytes: file.size ?? 0,
        owner: info.owner,
        path: info.path,
      });
    }

    if (attachments.length > 0) {
      onSelect(attachments);
    }
    onClose();
  };

  if (!open) return null;

  const ThemeWrapper = resolved === "dark" ? WillowDark : Willow;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/30" onClick={onClose}>
      <div
        className="bg-surface border border-border rounded-lg shadow-lg w-full max-w-3xl h-[70vh] flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-4 py-2.5 border-b border-border">
          <div className="flex items-center gap-2.5">
            <FolderOpenIcon className="h-5 w-5 text-accent" />
            <h3 className="text-base font-semibold text-text-primary">Browse Files</h3>
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={onClose}
              className="w-28 py-1.5 text-sm font-medium rounded-md border border-border text-text-secondary hover:text-text-primary hover:bg-surface-tertiary transition text-center"
            >
              Cancel
            </button>
            <button
              onClick={handleAttach}
              disabled={selectedCount === 0}
              className="w-28 py-1.5 text-sm font-medium rounded-md bg-accent text-surface hover:bg-accent-hover disabled:opacity-30 transition text-center"
            >
              Attach{selectedCount > 0 ? ` (${selectedCount})` : ""}
            </button>
          </div>
        </div>
        <div className="flex-1 min-h-0 filemanager-container">
          {loading && data.length === 0 ? (
            <div className="flex items-center justify-center h-full">
              <p className="text-sm text-text-tertiary">Loading files...</p>
            </div>
          ) : (
            <ThemeWrapper>
              <Locale words={{ filemanager: { "My files": " " } }}>
                <Filemanager
                  data={data}
                  init={handleInit}
                  mode="table"
                  menuOptions={() => []}
                />
              </Locale>
            </ThemeWrapper>
          )}
        </div>
      </div>
    </div>
  );
}
