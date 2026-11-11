---
id: read_memory_context
provider: memory
parameters:
  id:
    type: string
    description: Prompt-local procedural memory ID, such as m1.
required:
  - id
---
Read one procedural memory and the nearby source transcript using its prompt-local `id`.
Use this when the compact memory summary is insufficient to decide Playbook scope or
ownership.
