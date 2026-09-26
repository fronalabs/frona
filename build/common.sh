# Shared constants and helpers for build scripts.
# Source after cd-ing to the repo root.

# Callers configure publishing destinations; ordinary local releases retain defaults.
IMAGE="${FRONA_IMAGE:-ghcr.io/fronalabs/frona}"
SOURCE_URL="${FRONA_SOURCE_URL:-https://github.com/fronalabs/frona}"
DOCKERFILE="build/Dockerfile"
TARGET="prod"
PLATFORMS="${PLATFORM:-linux/amd64,linux/arm64}"
BUILDER="${FRONA_BUILDER:-multiarch}"

die() { echo "error: $*" >&2; exit 1; }

current_version() {
  grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/'
}

ensure_multiarch_builder() {
  docker buildx inspect "$BUILDER" >/dev/null 2>&1 || \
    docker buildx create --name "$BUILDER" --use
  docker buildx use "$BUILDER"
}

# Populates IMAGE_META_ARGS with --build-arg, --label, and --annotation flags.
# Args: version revision created
set_image_meta_args() {
  local version="$1" revision="$2" created="$3"
  IMAGE_META_ARGS=(
    --build-arg "VERSION=$version"
    --build-arg "REVISION=$revision"
    --build-arg "CREATED=$created"
    --label "org.opencontainers.image.source=$SOURCE_URL"
    --annotation "index:org.opencontainers.image.source=$SOURCE_URL"
    --annotation "index:org.opencontainers.image.description=Frona — personal AI assistant"
    --annotation "index:org.opencontainers.image.licenses=BSL-1.1"
    --annotation "index:org.opencontainers.image.title=frona"
    --annotation "index:org.opencontainers.image.version=$version"
    --annotation "index:org.opencontainers.image.revision=$revision"
    --annotation "index:org.opencontainers.image.created=$created"
  )
}
