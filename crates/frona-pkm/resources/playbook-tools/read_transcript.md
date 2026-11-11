---
id: read_transcript
provider: memory
parameters:
  cursor:
    type: string
    description: Prompt-local transcript cursor shown in the author context.
  direction:
    type: string
    enum: [before, after]
    description: Direction in which to expand the transcript.
  limit:
    type: integer
    minimum: 1
    maximum: 20
    description: Maximum number of contiguous messages to return.
required:
  - cursor
---
Read up to 20 contiguous messages around a prompt-local transcript `cursor`. Set
`direction` to `before` or `after`, and optionally set `limit` from 1 to 20. Both user and
agent messages are returned. At most 10 calls are available.
