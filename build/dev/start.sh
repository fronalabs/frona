#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

# Source scripts are bind-mounted, but development tools come from the image.
if ! command -v bacon >/dev/null 2>&1; then
  echo "Bacon is missing from the dev image. Rebuild it with: mise run container:dev:build" >&2
  exit 127
fi
if [[ ! -f bacon.toml ]]; then
  echo "Missing /app/bacon.toml. Recreate the dev container with the current Compose configuration." >&2
  exit 1
fi

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
  exec bash build/dev/run-command.sh 12 bacon --headless server-container
) &
pids+=("$!")

(
  cd web
  exec npm run dev
) &
pids+=("$!")

# If either watcher exits, stop its sibling rather than leave a partial stack.
wait -n
