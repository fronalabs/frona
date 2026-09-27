#!/usr/bin/env bash
set -euo pipefail
shutdown_timeout=$1
shift

# Bacon signals its immediate child. Give the whole watcher/build/server command a
# process group so a restart also stops Cargo, rustc, and script descendants.
set -m
child_pid=
cleanup() {
  trap '' INT TERM
  if [[ -n "$child_pid" ]]; then
    kill -TERM -- "-$child_pid" 2>/dev/null || true
    # The immediate child may exit before its descendants. Wait for the whole
    # group, ignoring zombies which cannot hold resources or handle signals.
    local deadline=$((SECONDS + shutdown_timeout))
    while ps -eo pgid=,stat= | awk -v group="$child_pid" \
      '$1 == group && $2 !~ /^Z/ { live = 1 } END { exit !live }'; do
      if (( SECONDS >= deadline )); then
        kill -KILL -- "-$child_pid" 2>/dev/null || true
        break
      fi
      sleep 0.1
    done
    wait "$child_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

"$@" &
child_pid=$!
wait "$child_pid"
