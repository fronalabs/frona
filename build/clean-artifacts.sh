#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Preserve dataset mount roots. Never select retained data/ or source build/.
for path in target web/target web/.next web/node_modules; do
  if [[ -L "$path" ]]; then
    printf 'Refusing artifact symlink: %s\n' "$path" >&2
    exit 1
  fi
  if [[ -d "$path" ]]; then
    find "$path" -mindepth 1 -xdev -delete
  elif [[ -e "$path" ]]; then
    printf 'Expected artifact directory: %s\n' "$path" >&2
    exit 1
  fi
done
