#!/usr/bin/env bash
set -euo pipefail

build_and_run() {
  # Downloads do not auto-start Kache's daemon; make the first compile a consumer.
  # Cache availability must never be required for a successful application build.
  if [[ "${RUSTC_WRAPPER:-}" == kache ]]; then
    kache daemon start || printf '%s\n' 'Kache daemon unavailable; compiling with local fallback.' >&2
  fi

  cargo build -p frona-cli-mcp
  cp target/debug/mcpctl /app/bin/mcpctl

  exec cargo run -p frona-server
}

# Own shutdown here so callers only need to launch this script. Sourcing the
# supervisor lets it run this function in a process group without another shell.
source "$(dirname "${BASH_SOURCE[0]}")/run-command.sh" 10 build_and_run
