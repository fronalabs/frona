#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
  cat <<'EOF'
Usage: build/container.sh PROFILE [COMMAND] [ARGS...]

PROFILE:
  dev | prod

COMMAND defaults to "up". Options beginning with "-" are passed to "up".
The runtime is selected with CONTAINER_RUNTIME, or detected (Podman first).
EOF
}

if [[ $# -lt 1 ]]; then
  usage >&2
  exit 2
fi

profile="$1"
shift
case "$profile" in
  dev | prod) ;;
  *)
    echo "Unknown profile: $profile" >&2
    usage >&2
    exit 2
    ;;
esac

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

action=up
if [[ $# -gt 0 && "$1" != -* ]]; then
  action="$1"
  shift
fi

# Match the development image user to the owner of bind-mounted source files.
# Callers may override these when the runtime uses a different ID mapping.
export CONTAINER_UID="${CONTAINER_UID:-$(id -u)}"
export CONTAINER_GID="${CONTAINER_GID:-$(id -g)}"
if [[ "$profile" == dev ]]; then
  export CONTAINER_RESTART_POLICY="${CONTAINER_RESTART_POLICY:-no}"
else
  export CONTAINER_RESTART_POLICY="${CONTAINER_RESTART_POLICY:-unless-stopped}"
fi

# podman-compose does not pass Podman's --jobs option through to image builds.
# Prebuild explicitly so independent Dockerfile stages can use all available CPUs.
if [[ "$runtime" == podman ]]; then
  build_jobs="${CONTAINER_BUILD_JOBS:-$(nproc)}"
  image="localhost/frona-${profile}:local"
  build_image() {
    local -a build_args=(
      build --layers --jobs "$build_jobs"
      --target "$profile"
      --tag "$image"
    )
    if [[ "$profile" == dev ]]; then
      build_args+=(
        --build-arg "DEV_UID=$CONTAINER_UID"
        --build-arg "DEV_GID=$CONTAINER_GID"
      )
    fi
    "$runtime" "${build_args[@]}" .
  }

  if [[ "$action" == build ]]; then
    build_image
    exit
  fi

  if [[ "$action" == up ]]; then
    filtered_args=()
    requested_build=false
    for arg in "$@"; do
      if [[ "$arg" == --build ]]; then
        requested_build=true
      else
        filtered_args+=("$arg")
      fi
    done
    if [[ "$requested_build" == true ]]; then
      build_image
      set -- "${filtered_args[@]}"
    fi
  fi
fi

# Podman requires bind-mount source paths to exist before container creation.
# Create them as the workspace user so outer-container tooling can write there.
mkdir -p data/browser_profiles web/node_modules web/.next web/target target/sccache

exec "$runtime" compose -f build/docker-compose.yml \
  --profile "$profile" "$action" "$@"
