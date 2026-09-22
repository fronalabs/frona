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

# Prebuild explicitly so both missing-image startup and requested rebuilds use
# parallel stages rather than podman-compose's default serial image build.
if [[ "$runtime" == podman ]]; then
  build_jobs="${CONTAINER_BUILD_JOBS:-$(nproc)}"
  image="localhost/frona-${profile}:local"
  build_image() {
    local -a build_args=(
      build --layers --jobs "$build_jobs" --file build/Dockerfile
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
    skip_build=false
    show_help=false
    for arg in "$@"; do
      case "$arg" in
        --build) requested_build=true ;;
        --no-build) skip_build=true; filtered_args+=("$arg") ;;
        -h | --help) show_help=true; filtered_args+=("$arg") ;;
        *) filtered_args+=("$arg") ;;
      esac
    done
    if [[ "$show_help" == false && "$skip_build" == false ]]; then
      if [[ "$requested_build" == true ]]; then
        build_image
      elif "$runtime" image exists "$image"; then
        : # Reuse the existing image without invoking the builder.
      else
        image_status=$?
        # Exit 1 means absent; storage/runtime failures must not trigger a build.
        if [[ "$image_status" -ne 1 ]]; then exit "$image_status"; fi
        build_image
      fi
      filtered_args=(--no-build ${filtered_args[@]+"${filtered_args[@]}"})
    fi
    set -- ${filtered_args[@]+"${filtered_args[@]}"}
  fi
fi

# Podman requires bind-mount source paths to exist before container creation.
# Create them as the workspace user so outer-container tooling can write there.
# Prefer dv's owner-private shared remote; standalone clones get their own.
if [[ -z "${KACHE_SHARED_DIR:-}" ]]; then
  if [[ -d /dv/shared/kache ]]; then
    export KACHE_SHARED_DIR=/dv/shared/kache
  else
    # Compose resolves relative bind sources from build/, not the repository root.
    export KACHE_SHARED_DIR="$PWD/target/kache-shared"
  fi
fi
# Keep the nested compiler cache across disposable workspace-image updates.
if [[ -z "${KACHE_LOCAL_DIR:-}" && -d /dv/private ]]; then
  export KACHE_LOCAL_DIR=/dv/private/kache/frona-podman
fi
if [[ "${KACHE_LOCAL_DIR:-}" == /* ]]; then
  mkdir -p "$KACHE_LOCAL_DIR"
fi
mkdir -p data/browser_profiles web/node_modules web/.next web/target "$KACHE_SHARED_DIR"

# podman-compose changes directory before opening files; -f paths must be absolute.
compose_args=(-f "$PWD/build/docker-compose.yml")
if [[ "$runtime" == podman && "$profile" == dev ]]; then
  # Match the image user to the workspace owner without changing other services.
  compose_args+=(-f "$PWD/build/dev/docker-compose.podman.yml")
fi
exec "$runtime" compose "${compose_args[@]}" --profile "$profile" "$action" "$@"
