---
name: shell
program: /bin/bash
args: ["-c", "{{command}}"]
timeout_secs: 30
parameters:
  command:
    type: string
    description: The shell command to execute
required:
  - command
---
Execute a shell command in the agent's sandboxed workspace. Standard Linux/macOS utilities are available (e.g. git, ls, find, grep, sed, awk, curl, tar, zip, jq, etc.).
