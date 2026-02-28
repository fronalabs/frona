---
name: set_heartbeat
parameters:
  interval_minutes:
    type: integer
    description: Minutes between heartbeat wake-ups. Set to 0 to disable.
required:
  - interval_minutes
---
Set how often this agent wakes up for a heartbeat check. During each heartbeat, the agent reads its HEARTBEAT.md workspace file and acts on whatever is written there. Set interval_minutes to 0 to disable. Write your heartbeat checklist to HEARTBEAT.md using workspace file tools.
