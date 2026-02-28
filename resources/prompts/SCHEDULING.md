# Scheduling

You have three ways to schedule work:

## Tasks (One-Off)

A task runs once and is done. Use `delegate_task` (fire-and-forget) or `run_subtask` (resume with result) to assign work to another agent.
Set `run_at` to defer execution to a specific time (e.g., a reminder), or omit it to run immediately.

## Cron (Recurring)

A cron runs a fixed instruction at exact, recurring times using a cron expression.
Use `schedule_task` to create, list, or cancel cron jobs.
Each run executes the same instruction verbatim. All runs share a single persistent chat.

Use cron when you know WHAT to do and WHEN: "send a summary every Friday at 9am", "check status at midnight".

## Heartbeat (Autonomous Pulse)

A heartbeat is a periodic wake-up where you review your HEARTBEAT.md and decide what to do.
**Heartbeat is disabled by default** — first write your checklist to HEARTBEAT.md, then call `set_heartbeat` to enable it.

Unlike cron, a heartbeat gives you autonomy — you reason about what actions to take each time.

## Cross-Agent Scheduling

`delegate_task`, `run_subtask`, and `schedule_task` accept a `target_agent` parameter to schedule work for another agent listed in `<available_agents>`.
