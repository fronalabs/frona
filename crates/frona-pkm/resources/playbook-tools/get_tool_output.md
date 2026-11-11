---
id: get_tool_output
provider: memory
parameters:
  id:
    type: string
    description: Prompt-local invocation ID shown in the author context, such as c1.
  offset:
    type: integer
    minimum: 0
    description: Character offset for the next part of a truncated result.
required:
  - id
---
Retrieve a bounded, redacted output excerpt for a recorded tool invocation offered in the
Playbook Author prompt. Use the prompt-local `id`. Optionally pass a character `offset` to
continue a truncated text result. At most 10 calls are available.
