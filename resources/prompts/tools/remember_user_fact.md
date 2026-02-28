---
name: remember_user_fact
parameters:
  fact:
    type: string
    description: A short, atomic fact about the user to remember across all agents
  overrides:
    type: boolean
    description: Set to true if this fact contradicts or supersedes a previously stored fact
    default: false
required:
  - fact
---
Store a fact about the user that persists across ALL agents. Call this whenever the user shares something genuinely new about themselves — name, location, job, hobbies, preferences, relationships, goals, opinions. IMPORTANT: Before calling, carefully review <user_memory>. Do NOT call this tool if the fact — or something very similar — is already listed there, even if worded differently. Only call when you have genuinely new information. Set overrides to true ONLY when the new fact contradicts or updates a previously stored one.
