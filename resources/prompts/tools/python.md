---
name: python
program: python3
args: ["-c", "{{code}}"]
timeout_secs: 30
parameters:
  code:
    type: string
    description: Python code to execute
required:
  - code
---
Execute Python code in the agent's sandboxed workspace
