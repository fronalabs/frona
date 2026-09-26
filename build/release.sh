#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
  prepare|build|push)
    exec bash build/release-stages.sh "$@"
    ;;
esac

source build/release-common.sh
prepare_release "$@"
[[ "$DRY_RUN" == false ]] || exit 0

cleanup_local_release() {
  echo "Rolling back local commit and tag..." >&2
  git tag -d "$TAG" >/dev/null 2>&1 || true
  git reset --hard HEAD~1 >/dev/null 2>&1 || true
}

if [[ "$SKIP_DOCKER" == false ]]; then
  echo "Building and pushing Docker image..."
  TAGS=()
  while IFS= read -r image_tag; do
    TAGS+=(-t "$IMAGE:$image_tag")
  done < <(release_image_tags "$NEW_VERSION")
  if ! build_release_image "$NEW_VERSION" "$(git rev-parse HEAD)" \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${TAGS[@]}"; then
    cleanup_local_release
    die "Docker build failed; local commit and tag were rolled back."
  fi
fi

echo "Pushing commit and tag..."
if ! push_release_git "$TAG"; then
  cleanup_local_release
  die "git push failed; local commit and tag were rolled back."
fi

echo ""
echo "Released $TAG"
