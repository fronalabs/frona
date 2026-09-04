---
id: create_recurring_task
provider: task
parameters:
  title:
    type: string
    description: A short title for the recurring task.
  instruction:
    type: string
    description: "Self-contained instruction for each fire. The agent cannot see this conversation, so include all necessary context. Omit timing language — the scheduler handles the when. Avoid embedding stale dates that would be wrong on later fires."
  cron_expression:
    type: string
    description: "5-field cron expression (minute hour day-of-month month day-of-week). Interpreted in the user's local timezone (see <temporal_context>). Write '0 8 * * *' for '8am every day' — the server handles UTC conversion and DST automatically."
  target_agent:
    type: string
    description: "Optional: agent name to assign each fire to (from <available_agents>). Omit to fire for yourself."
  timezone:
    type: string
    description: "Optional IANA timezone (e.g. 'America/Los_Angeles', 'Asia/Tokyo') overriding the default for cron_expression interpretation. Default is the user's local timezone. Set only when the user explicitly names a different zone — 'every weekday at 9am Tokyo time'."
  cron_mode:
    type: string
    description: "How fires relate to each other. 'singleton' (default): only the latest fire matters — older runs are hidden from the task tree by default. Pick this for reminders, polling, and 'tell me if X' patterns. 'per_instance': each fire is a distinct audited work item — every run is visible. Pick this for monthly reports, recurring payments, and any pattern where each fire must not overlap with the previous."
  cron_concurrency:
    type: string
    description: "What to do when a previous fire is still in flight. 'allow': spawn anyway, runs in parallel. 'forbid': skip the new fire while previous is running. 'replace': cancel the previous fire and start fresh. Defaults: singleton→replace, per_instance→forbid."
  process_result:
    type: boolean
    default: false
    description: "Continuation selector for every fire; it does not control result delivery. Omit it or use false when each fire performs the complete check, decision, action, and reporting itself. Use true only when the originating agent has a separate, concrete unfinished next step that requires every fire's result. Reminders, summaries, reports, polling, conditional alerts, target-agent delegation, and recurring execution do not by themselves require continuation. Complex nested result schemas require true."
  result_description:
    type: string
    description: "One-line description of the result each fire should produce. The executor fills `complete_task.result` with a prose/markdown string matching this description, and each fire renders directly into this chat. Use this for almost every recurring task; pass either this OR `result_schema`, not both. See examples below."
  result_schema:
    type: object
    description: "Advanced: JSON Schema describing the typed shape of each fire's `complete_task.result`. Simple scalar, list, and flat-object schemas can render directly and do not require continuation. Complex nested schemas require process_result=true so the originating agent can consume them. For prose results delivered to a human, prefer `result_description`. Pass either this OR `result_description`, not both."
required:
  - title
  - instruction
  - cron_expression
---
Create a recurring task that fires on a cron schedule. Use this for any work that should repeat: reminders, periodic polls, scheduled reports, regular check-ins.

Mode selection:
- "remind me to drink water every 2 minutes" → singleton + replace (newest fire matters; cancel older)
- "send me hourly news" → singleton + replace
- "generate a monthly report on the 1st" → per_instance + forbid (each report is its own audited work item; don't overlap)
- "process recurring payments daily at 9am" → per_instance + forbid

## Continuation decision

Every fire's result is delivered to the source chat whether `process_result` is false or true. Make the fire self-contained and use `false` for normal recurring work:

- "every hour, report AAPL only if it moved more than 5%" → `false`; the fire checks the threshold and returns either an alert or no result
- "every morning, summarize my calendar" → `false`; the summary is the final result
- "every Friday, generate a research report" → `false`; the report is delivered as-is

Use `true` only for a coordinated workflow where the originating agent must take a separate, concrete next step after every fire. Recurrence alone does not create that step.

## Describing each fire's result (required: pick one)

Tells the executing agent what to put in `complete_task.result` on each fire and shapes how each completion renders in this chat. Pass **exactly one** of:

### `result_description` (default — prose answer for a human)

Use for any recurring task whose fires are messages, summaries, or reports the user reads directly. One line describing what the executing agent should produce on each fire. The executor returns a markdown string and it renders as-is in this chat.

```text
result_description: "A short, friendly reminder to drink water"
result_description: "Top 5 headlines for today as markdown bullets"
result_description: "A one-line stock update for AAPL: current price and percent change"
```

This is the right choice for >90% of recurring tasks — reminders, daily summaries, news digests.

### `result_schema` (advanced — typed structure for programmatic use)

Use when each fire needs a machine-readable output shape. This choice is separate from continuation: simple schemas can render directly with `process_result: false`; set `true` when the originating agent must consume the fields or when the schema is complex. Pick the simplest shape that fits:

- **Always-notify scalar.**
  ```json
  { "type": "number", "description": "today's BTC closing price (USD)" }
  ```

- **Conditional notify** — nullable scalar. Pass `null` to skip the fire silently.
  ```json
  { "type": ["string", "null"], "description": "emergency text (null = no emergency)" }
  ```

- **List output** — array of scalars; renders as a bullet list.
  ```json
  { "type": "array", "items": { "type": "string" }, "description": "top headlines" }
  ```

- **Multi-field structured output** — object with scalar properties.
  ```json
  {
    "type": "object",
    "properties": {
      "symbol":     { "type": "string", "description": "ticker" },
      "price":      { "type": "number", "description": "current price (USD)" },
      "change_pct": { "type": "number", "description": "% change today" }
    },
    "required": ["symbol", "price", "change_pct"]
  }
  ```

- **Complex / nested schemas** — avoid unless you actually need them. If you must, set `process_result: true` and include a required top-level `summary` string property.

When in doubt, use `result_description`. Schemas express a typed output contract; they are not a reason by themselves to resume the originating agent.

For one-off or simply delayed work, use create_task. For periodic autonomous check-ins to your own state, use set_heartbeat.
