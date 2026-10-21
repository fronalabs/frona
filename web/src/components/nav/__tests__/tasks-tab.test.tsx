import React, { useEffect } from "react";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { TasksTab } from "../tasks-tab";
import { NavigationProvider, useNavigationRaw } from "@/lib/navigation-context";
import { SessionProvider } from "@/lib/session-context";
import { sseBus } from "@/lib/sse-event-bus";
import { api, deleteTask, getContacts, getCronRuns, getTask } from "@/lib/api-client";
import { CronRunsTable } from "@/components/chat/cron-runs-table";
import type { TaskKind, TaskResponse } from "@/lib/types";

const { push, addNotification } = vi.hoisted(() => ({ push: vi.fn(), addNotification: vi.fn() }));
const searchParams = new URLSearchParams();
vi.mock("next/navigation", () => ({ useRouter: () => ({ push }), useSearchParams: () => searchParams }));
vi.mock("@/lib/notification-context", () => ({ useNotifications: () => ({ addNotification }) }));
vi.mock("@/lib/api-client", async importOriginal => ({
  ...await importOriginal<typeof import("@/lib/api-client")>(),
  api: { get: vi.fn() }, getContacts: vi.fn(), getTask: vi.fn(), getCronRuns: vi.fn(), deleteTask: vi.fn(),
}));

const early = "2026-10-20T10:00:00Z";
const later = "2026-10-20T12:00:00Z";
const latest = "2026-10-20T14:00:00Z";
const cron: TaskKind = { type: "Cron", cron_expression: "0 * * * *", next_run_at: early };
function task(id: string, fields: Partial<TaskResponse> = {}): TaskResponse {
  return {
    id, title: id, agent_id: "system", space_id: null, chat_id: null,
    description: "", status: "pending", kind: { type: "Direct" }, run_at: early,
    result_summary: null, error_message: null, created_at: early, updated_at: early,
    ...fields,
  };
}
let initialTasks: TaskResponse[];
let navigation: ReturnType<typeof useNavigationRaw>;
function Boot({ children }: { children: React.ReactNode }) {
  const context = useNavigationRaw();
  useEffect(() => { navigation = context; }, [context]);
  const { refresh, tasks } = context;
  useEffect(() => { void refresh(); }, [refresh]);
  return tasks ? <SessionProvider>{children}</SessionProvider> : null;
}
async function mount(tasks: TaskResponse[], extra?: React.ReactNode) {
  initialTasks = tasks;
  const view = render(<NavigationProvider><Boot><TasksTab />{extra}</Boot></NavigationProvider>);
  await waitFor(() => expect(navigation.tasks).toBeDefined());
  return view;
}
function rows() {
  return screen.queryAllByRole("button").flatMap(button => {
    const title = button.querySelector("span.truncate")?.textContent;
    return title ? [title] : [];
  });
}
function update(task: TaskResponse) {
  act(() => sseBus.routeEvent("task_update", "", {
    task_id: task.id, status: task.status, title: task.title, chat_id: task.chat_id,
    source_chat_id: null, result_summary: task.result_summary,
  }));
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(res => { resolve = res; });
  return { promise, resolve };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.spyOn(sseBus, "connect").mockImplementation(() => {});
  vi.mocked(api.get).mockImplementation(async path => {
    if (path === "/api/navigation") return { spaces: [], standalone_chats: [] };
    if (path === "/api/tasks") return initialTasks;
    if (path === "/api/agents") return [{ id: "system", handle: "system", name: "System" }];
    throw new Error(`Unexpected request ${path}`);
  });
  vi.mocked(getContacts).mockResolvedValue([]);
  vi.mocked(getTask).mockRejectedValue(new Error("Task unavailable"));
  vi.mocked(deleteTask).mockResolvedValue(undefined);
});

describe("task list presentation", () => {
  it("ranks running, failed, due work, disabled schedules, terminal tasks, then unknown statuses", async () => {
    const tasks = [
      task("unknown", { status: "waiting_for_future_feature" }),
      task("done old", { status: "completed" }),
      task("disabled", { kind: cron, status: "cancelled" }),
      task("queued later", { run_at: later }),
      task("recurring early", { kind: cron }),
      task("failed", { status: "failed" }),
      task("cancelled newest", { status: "cancelled", updated_at: latest }),
      task("running", { status: "inprogress" }),
    ];
    const originalIds = tasks.map(task => task.id);
    await mount(tasks);
    expect(rows()).toEqual(["running", "failed", "recurring early", "queued later", "disabled", "cancelled newest", "done old", "unknown"]);
    expect(navigation.tasks?.map(task => task.id)).toEqual(originalIds);
    expect(tasks.map(task => task.id)).toEqual(originalIds);
  });

  const kinds: TaskKind[] = [
    { type: "Direct" },
    { type: "Delegation", source_agent_id: "agent", source_chat_id: "chat" },
    { type: "Signal", source_chat_id: "chat" },
    { ...cron, next_run_at: null },
  ];
  const statuses = [
    { status: "inprogress", position: 1 }, { status: "failed", position: 2 },
    { status: "pending", position: 3 }, { status: "cancelled", position: 5 },
    { status: "completed", position: 5 }, { status: "future_status", position: 6 },
  ];
  it.each(kinds.flatMap(kind => statuses.map(entry => ({ ...entry, kind, type: kind.type }))))(
    "ranks $type / $status using the same status rules", async ({ kind, status, position }) => {
      const anchors = [
        task("running", { status: "inprogress" }), task("failed", { status: "failed" }),
        task("due", { run_at: null }), task("disabled", { kind: { ...cron, next_run_at: null }, status: "cancelled" }),
        task("terminal", { status: "completed" }), task("unknown", { status: "unknown" }),
      ];
      await mount([...anchors, task("subject", { kind, status, run_at: null })]);
      const expected = anchors.map(task => task.id);
      expected.splice(kind.type === "Cron" && status === "cancelled" ? 4 : position, 0, "subject");
      expect(rows()).toEqual(expected);
    },
  );

  it("sorts valid dates before missing/invalid dates, preserving equal-date and undated ties", async () => {
    await mount([
      task("missing queued", { run_at: null }), task("invalid queued", { run_at: "invalid" }),
      task("queued late", { run_at: latest }), task("same instant queued", { run_at: "2026-10-20T11:00:00+01:00" }),
      task("same instant cron", { kind: cron }), task("missing cron", { kind: { ...cron, next_run_at: null } }),
      task("missing done", { status: "completed", updated_at: "" }),
      task("old cancelled", { status: "cancelled", updated_at: early }),
      task("new done", { status: "completed", updated_at: later }),
      task("same time cancelled", { status: "cancelled", updated_at: later }),
      task("invalid done", { status: "completed", updated_at: "invalid" }),
    ]);
    expect(rows()).toEqual([
      "same instant queued", "same instant cron", "queued late", "missing queued", "invalid queued", "missing cron",
      "new done", "same time cancelled", "old cancelled", "missing done", "invalid done",
    ]);
  });

  it.each([null, undefined, "", "invalid"])("puts a schedule with next_run_at=%s after dated queued work", async next_run_at => {
    await mount([task("schedule", { kind: { ...cron, next_run_at } }), task("queued", { run_at: later })]);
    expect(rows()).toEqual(["queued", "schedule"]);
  });

  it.each(statuses)("keeps $status CronRun children out of the top-level list and under their schedule", async ({ status }) => {
    const parent = task("schedule", { kind: cron });
    const run = task("child run", { status, chat_id: "run-chat", kind: { type: "CronRun", source_cron_id: parent.id, fire_at: early, sequence_num: 7 } });
    vi.mocked(getCronRuns).mockResolvedValue([run]);
    await mount([run, parent], <CronRunsTable cronId={parent.id} task={parent} />);
    expect(screen.queryByRole("button", { name: "child run" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /schedule/ })).toBeInTheDocument();
    expect(await within(await screen.findByRole("table")).findByText("#7")).toBeInTheDocument();
    expect(getCronRuns).toHaveBeenCalledWith(parent.id);
  });

  it("shows the empty state when only child runs exist", async () => {
    await mount([task("child", { kind: { type: "CronRun", source_cron_id: "parent", fire_at: early, sequence_num: 1 } })]);
    expect(screen.getByText("No active tasks")).toBeInTheDocument();
  });
});

describe("task list live updates", () => {
  it("refreshes date keys omitted by real SSE events without reordering the source array", async () => {
    const queued = task("queued", { run_at: latest });
    const recurring = task("schedule", { kind: { ...cron, next_run_at: later } });
    const done = task("done", { status: "completed" });
    await mount([queued, recurring, done, task("newer done", { status: "completed", updated_at: later })]);
    const order = () => rows();
    expect(order()).toEqual(["schedule", "queued", "newer done", "done"]);
    vi.mocked(getTask).mockResolvedValueOnce({ ...queued, run_at: early });
    update(queued);
    await waitFor(() => expect(order()).toEqual(["queued", "schedule", "newer done", "done"]));
    vi.mocked(getTask).mockResolvedValueOnce({ ...done, updated_at: latest });
    update(done);
    await waitFor(() => expect(order()).toEqual(["queued", "schedule", "done", "newer done"]));
    vi.mocked(getTask).mockResolvedValueOnce({ ...recurring, kind: { ...cron, next_run_at: "2026-10-20T09:00:00Z" } });
    update(recurring);
    await waitFor(() => expect(order()).toEqual(["schedule", "queued", "done", "newer done"]));
    expect(navigation.tasks?.map(task => task.id)).toEqual(["queued", "schedule", "done", "newer done"]);
  });

  it("preserves undated groups and ties across unrelated updates and new arrivals", async () => {
    const tasks = [
      task("running B", { status: "inprogress" }), task("running A", { status: "inprogress" }),
      task("failed B", { status: "failed" }), task("failed A", { status: "failed" }),
      task("queued B"), task("queued A"),
      task("disabled B", { kind: { ...cron, next_run_at: null }, status: "cancelled" }),
      task("disabled A", { kind: { ...cron, next_run_at: null }, status: "cancelled" }),
      task("done B", { status: "completed" }), task("done A", { status: "completed" }),
      task("future B", { status: "future" }), task("future A", { status: "future" }),
    ];
    await mount(tasks);
    const before = rows();
    for (const original of [...tasks].reverse()) {
      vi.mocked(getTask).mockResolvedValueOnce({ ...original, result_summary: "Updated" });
      update(original);
      await waitFor(() => expect(navigation.tasks?.find(task => task.id === original.id)?.result_summary).toBe("Updated"));
      expect(rows()).toEqual(before);
    }
    const arrival = task("new task", { status: "inprogress" });
    vi.mocked(getTask).mockResolvedValueOnce(arrival);
    update(arrival);
    await waitFor(() => expect(rows()).toEqual(["new task", ...before]));
  });

  it("keeps the event status when refresh fails and recovers on the next event", async () => {
    const item = task("task", { status: "completed" });
    await mount([item, task("failed", { status: "failed" })]);
    update({ ...item, status: "inprogress" });
    await act(() => Promise.resolve());
    expect(rows()).toEqual(["task", "failed"]);
    const recovered = { ...item, status: "pending", run_at: latest };
    vi.mocked(getTask).mockResolvedValueOnce(recovered);
    update(recovered);
    await waitFor(() => expect(navigation.tasks?.[0]).toEqual(recovered));
    expect(rows()).toEqual(["failed", "task"]);
  });

  it("ignores older fetches that finish after a newer event, including a failed newer refresh", async () => {
    const item = task("task");
    await mount([item]);
    const old = deferred<TaskResponse>();
    vi.mocked(getTask).mockReturnValueOnce(old.promise);
    update({ ...item, status: "inprogress" });
    const latestTask = { ...item, status: "completed", updated_at: latest };
    vi.mocked(getTask).mockResolvedValueOnce(latestTask);
    update(latestTask);
    await waitFor(() => expect(navigation.tasks?.[0]).toEqual(latestTask));
    await act(async () => old.resolve({ ...item, status: "inprogress" }));
    expect(navigation.tasks?.[0]).toEqual(latestTask);

    const oldAgain = deferred<TaskResponse>();
    vi.mocked(getTask).mockReturnValueOnce(oldAgain.promise);
    update(item);
    update({ ...item, status: "failed" });
    await act(async () => oldAgain.resolve(item));
    expect(navigation.tasks?.[0].status).toBe("failed");
  });

  it("does not resurrect a deleted task when its pending refresh completes", async () => {
    const item = task("task", { status: "completed" });
    await mount([item]);
    const pending = deferred<TaskResponse>();
    vi.mocked(getTask).mockReturnValueOnce(pending.promise);
    update(item);
    await act(async () => navigation.deleteTask(item.id));
    await act(async () => pending.resolve(item));
    expect(screen.getByText("No active tasks")).toBeInTheDocument();
    expect(navigation.tasks).toEqual([]);
  });
});
