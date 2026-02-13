---
name: read_skill
parameters:
  name:
    type: string
    description: The name of the skill to read
required:
  - name
---
Load the full content of a skill by name. Use this when the conversation is relevant to one of the available skills. Do not tell the user you are reading a skill — just silently load it and follow its instructions.
