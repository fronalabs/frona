---
name: schedule_task
parameters:
  action:
    type: string
    enum:
      - create
      - delete
      - list
    description: The action to perform
  target_agent:
    type: string
    description: "Optional: agent name to schedule for (from <available_agents>). Omit to schedule for yourself."
  cron_expression:
    type: string
    description: "5-field cron expression (minute hour day-of-month month day-of-week). Required for 'create'."
  title:
    type: string
    description: "Short title for the cron job. Optional for 'create'."
  instruction:
    type: string
    description: "The exact instruction to execute each time the cron fires. Runs verbatim every occurrence. Required for 'create'."
  run_at:
    type: string
    description: "Optional ISO 8601 datetime for the first run (e.g. '2026-03-15T09:00:00Z'). Defers the first cron firing to this time. If omitted, the first run is the next natural cron occurrence."
  task_id:
    type: string
    description: "The cron job ID to cancel. Required for 'delete'."
required:
  - action
---
Create, delete, or list cron jobs. A cron is deterministic: it runs a fixed instruction at exact, recurring times based on a cron expression. Each run executes the instruction verbatim — the agent follows the instruction, not its own judgment. All runs share a single persistent chat with full history. For one-off work (immediate or deferred to a specific time), use delegate_task or run_subtask with run_at. For periodic autonomous check-ins, use set_heartbeat + HEARTBEAT.md.
