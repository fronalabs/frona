"use client";

import { useNavigation } from "@/lib/navigation-context";
import { TaskItem } from "./task-item";

export function TasksTab() {
  const { tasks } = useNavigation();

  return (
    <div className="space-y-1 p-2">
      {tasks.map((task) => (
        <TaskItem key={task.id} task={task} />
      ))}
      {tasks.length === 0 && (
        <p className="px-2 py-4 text-center text-xs text-text-tertiary">
          No active tasks
        </p>
      )}
    </div>
  );
}
