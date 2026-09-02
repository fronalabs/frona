import { CreateTaskView } from "./create-task";
import { DefaultView } from "./default";
import { DeleteTaskView } from "./delete-task";
import { FileView } from "./file";
import { HeartbeatView } from "./heartbeat";
import {
  memoryDefaultExpanded,
  StoreAgentMemoryView,
  StoreUserMemoryView,
} from "./memory";
import { MemoryGraphSparqlView } from "./memory-graph-sparql";
import { MemorySearchView } from "./memory-search";
import { NodeView } from "./node";
import { ProduceFileView } from "./produce-file";
import { PythonView } from "./python";
import { RecurringTaskView } from "./recurring-task";
import { ShellView } from "./shell";
import { UpdateIdentityView } from "./update-identity";
import { WebFetchView } from "./web-fetch";
import { WebSearchView } from "./web-search";
import type { ToolView } from "./types";

export type ToolMatcher = { match: (toolName: string) => boolean; view: ToolView };

export const TOOL_VIEWS_EXACT: Record<string, ToolView> = {
  shell: ShellView,
  python: PythonView,
  node: NodeView,
  read: FileView,
  write: FileView,
  edit: FileView,
  glob: FileView,
  grep: FileView,
  produce_file: ProduceFileView,
  web_search: WebSearchView,
  web_fetch: WebFetchView,
  create_task: CreateTaskView,
  create_recurring_task: RecurringTaskView,
  delete_task: DeleteTaskView,
  store_agent_memory: StoreAgentMemoryView,
  store_user_memory: StoreUserMemoryView,
  memory_remember: StoreAgentMemoryView,
  memory_search: MemorySearchView,
  memory_graph_sparql: MemoryGraphSparqlView,
  update_identity: UpdateIdentityView,
  set_heartbeat: HeartbeatView,
};

export const TOOL_VIEWS_PATTERN: ToolMatcher[] = [];

/**
 * Per-tool predicate that decides whether the row should start expanded.
 * Called once at mount with the tool-call args. Omitting an entry keeps the
 * default behavior (start collapsed).
 */
export const TOOL_VIEWS_DEFAULT_EXPANDED: Record<string, (args: unknown) => boolean> = {
  store_agent_memory: memoryDefaultExpanded,
  store_user_memory: memoryDefaultExpanded,
  memory_remember: memoryDefaultExpanded,
};

export function pickView(toolName: string): ToolView {
  const exact = TOOL_VIEWS_EXACT[toolName];
  if (exact) return exact;
  for (const { match, view } of TOOL_VIEWS_PATTERN) {
    if (match(toolName)) return view;
  }
  return DefaultView;
}

export { DefaultView } from "./default";
export type { ToolView, ToolViewProps } from "./types";
