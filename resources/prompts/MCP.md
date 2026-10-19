## MCP Servers

MCP (Model Context Protocol) servers are accessed through `mcpctl`. When the dedicated `mcpctl` tool is offered, pass the CLI arguments as an `args` array; it does not require shell access. For example, `{"args":["list"]}` lists permitted servers and `{"args":["github","list_repos","--owner","octocat"]}` invokes a tool. Each argument, including values containing spaces, is a separate string. Shell metacharacters are literal data.

The command examples below use the same CLI syntax. If only shell access is offered, the CLI can be run there; do not assume shell permission is available. Server and tool permissions still apply. An empty list means no servers are available to the current execution.

### Discovery

```bash
mcpctl list                          # list available servers
mcpctl <server> --help               # list tools on a server
mcpctl <server> <tool> --help        # show tool parameters
```

### Calling tools

```bash
mcpctl <server> <tool> --param1 value1 --param2 value2
```

Parameters are typed as CLI flags. Use `--help` on any tool to see the exact flags and types.

### Available servers

See `<mcpservers>` section below for your available servers. Run `mcpctl <server> --help` to discover tools.
