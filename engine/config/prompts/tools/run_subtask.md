---
name: run_subtask
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
Run a subtask on another agent and resume when it completes. Unlike delegate_task, the result is returned to YOU (the calling agent) so you can process it further. Use this when you need the sub-agent's output to continue your work. Optionally set run_at to defer execution.
