#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

# Give each watcher its own process group so shutdown also reaches its children.
set -m
pids=()

cleanup() {
  trap '' INT TERM
  for pid in "${pids[@]}"; do
    kill -TERM -- "-$pid" 2>/dev/null || true
  done
  wait || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

(
  cd web
  npm install
  cd ..
  exec cargo watch -w crates/ -w Cargo.toml -w Cargo.lock --delay 3 -s /app/build/dev/watch.sh
) &
pids+=("$!")

(
  cd web
  exec npm run dev
) &
pids+=("$!")

# If either watcher exits, stop its sibling rather than leave a partial stack.
wait -n
