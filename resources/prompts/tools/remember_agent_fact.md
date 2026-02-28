---
name: remember_agent_fact
parameters:
  fact:
    type: string
    description: A short, atomic fact about the user to remember
  overrides:
    type: boolean
    description: Set to true if this fact contradicts or supersedes a previously stored fact
    default: false
required:
  - fact
---
Store an insight for this agent's long-term memory. IMPORTANT: Before calling, carefully review <agent_memory>. Do NOT call this tool if the insight — or something very similar — is already listed there, even if worded differently. Each insight should be a short, atomic statement — working context, project details, decisions, or anything relevant to this agent's work. Set overrides to true ONLY when the new insight contradicts or updates a previously stored one.
