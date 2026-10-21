"use client";

import { useNavigation } from "@/lib/navigation-context";
import type { TaskResponse } from "@/lib/types";
import { TaskItem } from "./task-item";

function taskSortKey(task: TaskResponse): { rank: number; time: number } {
  switch (task.status) {
    case "inprogress": return { rank: 0, time: 0 };
    case "failed": return { rank: 1, time: 0 };
    case "pending": {
      const date = task.kind.type === "Cron" ? task.kind.next_run_at : task.run_at;
      const time = date ? Date.parse(date) : NaN;
      return { rank: 2, time: Number.isFinite(time) ? time : Infinity };
    }
    case "cancelled":
      if (task.kind.type === "Cron") return { rank: 3, time: 0 };
      // Fall through to terminal ordering for one-off tasks.
    case "completed": {
      // The task API has no separate completion/cancellation timestamp.
      const time = Date.parse(task.updated_at);
      return { rank: 4, time: Number.isFinite(time) ? -time : Infinity };
    }
    default: return { rank: 5, time: 0 };
  }
}

export function TasksTab() {
  const { tasks } = useNavigation();
  const visibleTasks = tasks
    .filter(task => task.kind.type !== "CronRun")
    .sort((a, b) => {
      const aKey = taskSortKey(a);
      const bKey = taskSortKey(b);
      // Stable sort retains source order for equal keys and undated groups.
      return aKey.rank - bKey.rank || (aKey.time === bKey.time ? 0 : aKey.time - bKey.time);
    });

  return (
    <div className="space-y-1 p-2">
      {visibleTasks.map((task) => (
        <TaskItem key={task.id} task={task} />
      ))}
      {visibleTasks.length === 0 && (
        <p className="px-2 py-4 text-center text-xs text-text-tertiary">
          No active tasks
        </p>
      )}
    </div>
  );
}
