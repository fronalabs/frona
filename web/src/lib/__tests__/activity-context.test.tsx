import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ChatSSEEvent } from "../sse-event-bus";
import type { ActivitySnapshot, TaskResponse } from "../types";

const mocks = vi.hoisted(() => ({
  getActivity: vi.fn<() => Promise<ActivitySnapshot>>(),
  globalListener: null as ((event: { type: string }) => void) | null,
  chatEventListener: null as ((chatId: string, event: ChatSSEEvent) => void) | null,
  reconnectListener: null as (() => void) | null,
  tasks: [] as TaskResponse[],
}));

vi.mock("../api-client", () => ({
  getActivity: mocks.getActivity,
}));

vi.mock("../sse-event-bus", () => ({
  sseBus: {
    onGlobal: vi.fn((listener: (event: { type: string }) => void) => {
      mocks.globalListener = listener;
      return () => { mocks.globalListener = null; };
    }),
    onChatEvent: vi.fn((listener: (chatId: string, event: ChatSSEEvent) => void) => {
      mocks.chatEventListener = listener;
      return () => { mocks.chatEventListener = null; };
    }),
    onReconnect: vi.fn((listener: () => void) => {
      mocks.reconnectListener = listener;
      return () => { mocks.reconnectListener = null; };
    }),
  },
}));

vi.mock("../navigation-context", () => ({
  useNavigation: () => ({ tasks: mocks.tasks }),
}));

import { ActivityProvider, useActivity } from "../activity-context";

const activeSnapshot: ActivitySnapshot = {
  executions: [
    {
      id: "execution-1",
      title: "Memory consolidation",
      kind: "memory",
      status: "running",
      startedAt: "2026-09-02T12:00:00Z",
      canCancel: false,
    },
  ],
};

const taskSnapshot: ActivitySnapshot = {
  executions: [
    {
      id: "execution-2",
      title: "Research prices",
      kind: "task",
      status: "running",
      action: "Running task",
      source: { type: "task", id: "task-1" },
      relatedChatIds: ["source-chat"],
      startedAt: "2026-09-02T12:00:00Z",
      canCancel: true,
    },
  ],
};

function Count() {
  const { snapshot } = useActivity();
  return <span>{snapshot.executions.length}</span>;
}

function FirstAction() {
  const { snapshot } = useActivity();
  return <span>{snapshot.executions[0]?.action ?? "none"}</span>;
}

async function flushPromises() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("ActivityProvider", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    mocks.getActivity.mockReset();
    mocks.globalListener = null;
    mocks.chatEventListener = null;
    mocks.reconnectListener = null;
    mocks.tasks = [];
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("polls every 20 seconds only while an execution is active", async () => {
    mocks.getActivity
      .mockResolvedValueOnce(activeSnapshot)
      .mockResolvedValueOnce({ executions: [] });

    render(
      <ActivityProvider>
        <Count />
      </ActivityProvider>,
    );
    await flushPromises();

    expect(screen.getByText("1")).toBeInTheDocument();
    expect(mocks.getActivity).toHaveBeenCalledTimes(1);

    await act(async () => vi.advanceTimersByTime(20_000));
    await flushPromises();

    expect(screen.getByText("0")).toBeInTheDocument();
    expect(mocks.getActivity).toHaveBeenCalledTimes(2);

    await act(async () => vi.advanceTimersByTime(20_000));
    expect(mocks.getActivity).toHaveBeenCalledTimes(2);
  });

  it("refreshes immediately on an SSE wake event and cancels the fallback poll", async () => {
    mocks.getActivity
      .mockResolvedValueOnce(activeSnapshot)
      .mockResolvedValueOnce({ executions: [] });

    render(
      <ActivityProvider>
        <Count />
      </ActivityProvider>,
    );
    await flushPromises();

    act(() => mocks.globalListener?.({ type: "activity_changed" }));
    await flushPromises();

    expect(screen.getByText("0")).toBeInTheDocument();
    expect(mocks.getActivity).toHaveBeenCalledTimes(2);

    await act(async () => vi.advanceTimersByTime(20_000));
    expect(mocks.getActivity).toHaveBeenCalledTimes(2);
  });

  it("shows task tool progress from the task's private chat across snapshot polls", async () => {
    mocks.tasks = [
      {
        id: "task-1",
        agent_id: "agent-1",
        space_id: null,
        chat_id: "task-chat",
        title: "Research prices",
        description: "",
        status: "inprogress",
        kind: { type: "Direct" },
        run_at: null,
        result_summary: null,
        error_message: null,
        created_at: "2026-09-02T12:00:00Z",
        updated_at: "2026-09-02T12:00:00Z",
      },
    ];
    mocks.getActivity
      .mockResolvedValueOnce(taskSnapshot)
      .mockResolvedValueOnce(taskSnapshot);

    render(
      <ActivityProvider>
        <FirstAction />
      </ActivityProvider>,
    );
    await flushPromises();

    act(() => {
      mocks.chatEventListener?.("task-chat", {
        type: "tool_call",
        id: "tool-1",
        provider_call_id: "provider-tool-1",
        name: "web_search",
        arguments: { query: "prices" },
        description: "Searching current prices",
      });
    });
    expect(screen.getByText("Searching current prices")).toBeInTheDocument();

    await act(async () => vi.advanceTimersByTime(20_000));
    await flushPromises();

    expect(screen.getByText("Searching current prices")).toBeInTheDocument();
  });
});
