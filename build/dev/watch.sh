#!/usr/bin/env bash
set -e

# Downloads do not auto-start Kache's daemon; make the first compile a consumer.
# Cache availability must never be required for a successful application build.
if [[ "${RUSTC_WRAPPER:-}" == kache ]]; then
  kache daemon start || printf '%s\n' 'Kache daemon unavailable; compiling with local fallback.' >&2
fi

cargo build -p frona-cli-mcp
cp target/debug/mcpctl /app/bin/mcpctl

cargo run -p frona-server
