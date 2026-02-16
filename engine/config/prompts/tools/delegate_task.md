---
name: delegate_task
parameters:
  target_agent:
    type: string
    description: The name of the agent to delegate the task to (from <available_agents>)
  title:
    type: string
    description: A short title for the task
  instruction:
    type: string
    description: Detailed instructions for the target agent
  run_at:
    type: string
    description: "Optional ISO 8601 datetime to defer execution (e.g., '2026-03-15T14:00:00Z'). If omitted, the task runs immediately."
required:
  - target_agent
  - title
  - instruction
---
The default delegation tool. Delegate a task to another agent — fire-and-forget. The result is posted directly to this chat for the user; your tool loop is NOT resumed. Returns immediately with a task ID. Optionally set run_at to defer execution. For recurring scheduled work, use schedule_task. For periodic autonomous check-ins, use set_heartbeat.
