# Shared implementation for local and staged releases. Source from the repo root.
source build/common.sh

CARGO_TOML="Cargo.toml"
CARGO_LOCK="Cargo.lock"
PKG_JSON="web/package.json"
PKG_LOCK="web/package-lock.json"

parse_version() {
  local ver="$1"
  local base pre
  if [[ "$ver" == *-* ]]; then
    base="${ver%%-*}"
    pre="${ver#*-}"
  else
    base="$ver"
    pre=""
  fi
  IFS='.' read -r MAJOR MINOR PATCH <<< "$base"
  PRE_TAG="" PRE_NUM=""
  if [[ -n "$pre" ]]; then
    PRE_TAG="${pre%%[0-9]*}"
    PRE_NUM=$(echo "$pre" | grep -o '[0-9]*$')
  fi
}

format_version() {
  local ver="${MAJOR}.${MINOR}.${PATCH}"
  if [[ -n "${PRE_TAG:-}" ]]; then
    ver="${ver}-${PRE_TAG}${PRE_NUM}"
  fi
  echo "$ver"
}

today_calver() {
  date +%Y.%-m
}

roll_to_today() {
  local today today_year today_month
  today=$(today_calver)
  today_year="${today%.*}"
  today_month="${today#*.}"
  if [[ "$MAJOR" == "$today_year" && "$MINOR" == "$today_month" ]]; then
    PATCH=$((PATCH + 1))
  else
    MAJOR="$today_year"
    MINOR="$today_month"
    PATCH=0
  fi
}

update_files() {
  local new_ver="$1"
  local old_ver
  old_ver=$(current_version)

  sed -i.bak "s/^version = \"${old_ver}\"/version = \"${new_ver}\"/" "$CARGO_TOML"
  rm -f "${CARGO_TOML}.bak"

  sed -i.bak "s/\"version\": \"${old_ver}\"/\"version\": \"${new_ver}\"/" "$PKG_JSON"
  rm -f "${PKG_JSON}.bak"

  sed -i.bak "s/\"version\": \"${old_ver}\"/\"version\": \"${new_ver}\"/" "$PKG_LOCK"
  rm -f "${PKG_LOCK}.bak"
}

prepare_release() {
  DRY_RUN=false
  SKIP_DOCKER=false
  SKIP_TESTS=false
  COMMAND=""
  PRE_RELEASE_TYPE=""

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dry-run)    DRY_RUN=true; shift ;;
      --skip-docker) SKIP_DOCKER=true; shift ;;
      --skip-tests) SKIP_TESTS=true; shift ;;
      -*)           die "Unknown flag: $1" ;;
      *)
        if [[ -z "$COMMAND" ]]; then
          COMMAND="$1"
        elif [[ -z "$PRE_RELEASE_TYPE" ]]; then
          PRE_RELEASE_TYPE="$1"
        else
          die "Unexpected argument: $1"
        fi
        shift
        ;;
    esac
  done

  USAGE="Usage: release.sh [command] [--dry-run] [--skip-docker] [--skip-tests]

  Frona uses CalVer YYYY.M.PATCH (year.month.in-month-patch).

  Commands:
    (no arg) / today            Cut today's release; bumps PATCH within the current month
                                or resets to YYYY.M.0 when the month rolls over
    patch                       Increment PATCH within the current YYYY.M (strict — errors
                                if the calendar month has changed since the last release)
    alpha / beta / rc           Start or advance a pre-release; rolls to today's YYYY.M
                                if current version is already stable
    stable                      Promote current pre-release to stable
    <version>                   Set exact version (e.g., 2026.5.0-RC1)"

  OLD_VERSION=$(current_version)
  parse_version "$OLD_VERSION"

  case "$COMMAND" in
    ""|today)
      [[ -z "$PRE_RELEASE_TYPE" ]] || die "Pre-releases are not supported here. Use 'alpha', 'beta', or 'rc' to start one."
      if [[ -n "$PRE_TAG" ]]; then
        die "Current version is a pre-release ($OLD_VERSION). Use 'stable' to promote or 'alpha'/'beta'/'rc' to advance."
      fi
      roll_to_today
      PRE_TAG="" PRE_NUM=""
      ;;
    patch)
      [[ -z "$PRE_RELEASE_TYPE" ]] || die "Pre-releases are not supported for patch bumps. Use 'alpha', 'beta', or 'rc'."
      if [[ -n "$PRE_TAG" ]]; then
        die "Current version is a pre-release ($OLD_VERSION). Use 'stable' to promote or 'alpha'/'beta'/'rc' to advance."
      fi
      today=$(today_calver)
      today_year="${today%.*}"
      today_month="${today#*.}"
      if [[ "$MAJOR" != "$today_year" || "$MINOR" != "$today_month" ]]; then
        die "Current version ($OLD_VERSION) is in a previous month. Run 'mise run release' (no arg) to cut ${today}.0 for this month."
      fi
      PATCH=$((PATCH + 1))
      ;;
    alpha|beta|rc)
      REQUESTED_TAG=$(echo "$COMMAND" | tr '[:lower:]' '[:upper:]')
      if [[ -z "$PRE_TAG" ]]; then
        roll_to_today
        PRE_TAG="$REQUESTED_TAG"
        PRE_NUM=1
      elif [[ "$PRE_TAG" == "$REQUESTED_TAG" ]]; then
        PRE_NUM=$((PRE_NUM + 1))
      else
        PRE_TAG="$REQUESTED_TAG"
        PRE_NUM=1
      fi
      ;;
    stable)
      [[ -n "$PRE_TAG" ]] || die "Current version ($OLD_VERSION) is already stable."
      PRE_TAG="" PRE_NUM=""
      ;;
    *)
      if [[ "$COMMAND" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z]+[0-9]+)?$ ]]; then
        parse_version "$COMMAND"
        if [[ -n "$PRE_TAG" ]]; then
          PRE_TAG=$(echo "$PRE_TAG" | tr '[:lower:]' '[:upper:]')
        fi
      else
        die "Invalid version or command: $COMMAND

  $USAGE"
      fi
      ;;
  esac

  NEW_VERSION=$(format_version)
  IS_PRERELEASE=false
  [[ -n "${PRE_TAG:-}" ]] && IS_PRERELEASE=true

  echo "Version: $OLD_VERSION → $NEW_VERSION"
  echo "Pre-release: $IS_PRERELEASE"

  if [[ -n "$(git status --porcelain)" ]]; then
    die "Working tree is not clean. Commit or stash changes first."
  fi

  if [[ "$IS_PRERELEASE" == "false" ]]; then
    BRANCH=$(git rev-parse --abbrev-ref HEAD)
    [[ "$BRANCH" == "main" ]] || die "Stable releases must be from main branch (currently on '$BRANCH')."
  fi

  TAG="v${NEW_VERSION}"
  if git rev-parse "$TAG" >/dev/null 2>&1; then
    die "Tag $TAG already exists."
  fi

  if [[ "$DRY_RUN" == "true" ]]; then
    echo ""
    echo "Dry run — no changes will be made."
    echo ""
    echo "  Version:  $OLD_VERSION → $NEW_VERSION"
    echo "  Tag:      $TAG"
    echo "  Commit:   release: $TAG"
    if [[ "$IS_PRERELEASE" == "false" ]]; then
      echo "  Docker:   $IMAGE:$TAG, $IMAGE:latest"
    else
      echo "  Docker:   $IMAGE:$TAG"
    fi
    echo ""
    echo "  Files:"
    echo "    $CARGO_TOML"
    echo "    $CARGO_LOCK"
    echo "    $PKG_JSON"
    echo "    $PKG_LOCK"
    return 0
  fi

  if [[ "$SKIP_TESTS" == "false" ]]; then
    echo "Running tests..."
    cargo test --workspace
  fi

  echo "Updating version files..."
  update_files "$NEW_VERSION"

  echo "Syncing Cargo.lock to new workspace version..."
  cargo update --workspace --offline

  echo "Committing and tagging locally..."
  git add "$CARGO_TOML" "$CARGO_LOCK" "$PKG_JSON" "$PKG_LOCK"
  git commit -m "release: $TAG"
  git tag -a "$TAG" -m "Release $TAG"

}

release_image_tags() {
  echo "v$1"
  [[ "$1" == *-* ]] || echo latest
}

build_release_image() {
  local version="$1" revision="$2" created="$3"
  shift 3
  ensure_multiarch_builder || return
  set_image_meta_args "$version" "$revision" "$created"
  docker buildx build --platform "$PLATFORMS" \
    -f "$DOCKERFILE" --target "$TARGET" \
    "${IMAGE_META_ARGS[@]}" "$@" --push .
}

push_release_git() {
  git push --atomic origin HEAD "refs/tags/$1"
}
