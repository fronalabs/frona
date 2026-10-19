---
id: mcpctl
provider: mcpctl
parameters:
  args:
    type: array
    items:
      type: string
    description: 'Arguments after mcpctl, preserving each argument separately. Examples: ["list"], ["github", "--help"], ["github", "list_repos", "--owner", "octocat"].'
required:
  - args
---
Run the mcpctl CLI bridge using its exact command syntax, help, flags, output and errors. No shell permission is required. Use args=[] or ["--help"] for usage, ["list"] for permitted servers, ["server", "--help"] for tools, and ["server", "tool", "--help"] for parameters. Pass spaces and shell metacharacters as literal argument data; nothing is executed by a shell. Frona uses the current agent and user context. Operations remain limited to permitted MCP server tools. Output is bounded and calls time out after at most 120 seconds. An empty server list means no servers are currently available to this execution.
