#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
runtime="${CONTAINER_RUNTIME:-}"
if [[ -z "$runtime" ]]; then
  if command -v podman >/dev/null 2>&1; then
    runtime=podman
  elif command -v docker >/dev/null 2>&1; then
    runtime=docker
  else
    echo "Neither Podman nor Docker is installed." >&2
    exit 1
  fi
fi
if ! command -v "$runtime" >/dev/null 2>&1; then
  echo "Container runtime not found: $runtime" >&2
  exit 1
fi

for tool in curl cargo; do
  command -v "$tool" >/dev/null || { echo "Required command not found: $tool" >&2; exit 1; }
done

# Use the same pinned image as the development stack.
image=$(awk '$1 == "image:" && $2 ~ /^ghcr.io\/browserless\/chromium:/ { print $2; exit }' build/docker-compose.yml)
: "${image:?Browserless image missing from build/docker-compose.yml}"
container_name="frona-browser-tests-$$-$RANDOM"
cleanup() {
  local status=$?
  if "$runtime" container inspect "$container_name" >/dev/null 2>&1; then
    if [[ "$status" != 0 ]]; then
      "$runtime" logs --tail 100 "$container_name" >&2 || true
    fi
    "$runtime" rm --force --volumes "$container_name" >/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Isolate destructive session tests from the development browser and its profiles.
"$runtime" run --detach --name "$container_name" \
  --publish 127.0.0.1::3333 --shm-size=1g \
  --env PORT=3333 --env TIMEOUT=1800000 \
  --env MAX_CONCURRENT_SESSIONS=10 --env ENABLE_DEBUGGER=true \
  --tmpfs /profiles:rw,mode=1777 "$image" >/dev/null
address=$("$runtime" port "$container_name" 3333/tcp)
export FRONA_TEST_BROWSER_WS_URL="ws://$address"

ready=false
for ((attempt = 0; attempt < 60; attempt++)); do
  if curl --fail --silent --max-time 1 "http://$address/json/version" >/dev/null; then
    ready=true
    break
  fi
  sleep 1
done
if [[ "$ready" != true ]]; then
  echo "Browserless did not become ready at $address" >&2
  exit 1
fi

echo "Running browser tests against $FRONA_TEST_BROWSER_WS_URL"
# These tests kill Browserless sessions, so keep them serial.
cargo test -p frona-browser -- --include-ignored --test-threads=1
cargo test -p frona-server --lib tool::browser::session::tests -- --include-ignored --test-threads=1
cargo test -p frona-server --test browser_sessions -- --test-threads=1
