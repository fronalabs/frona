#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd -P)
source build/release-common.sh
command -v jq >/dev/null || die 'Staged releases require jq'

STAGE=${1:?Expected prepare, build, or push}
shift
OUTPUT= ARTIFACT= ARCH= VERSION_COMMAND=today
PREPARE_FLAGS=() RESULTS=()
version_set=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output|--release|--arch)
      [[ $# -ge 2 && -n "$2" ]] || die "Missing value for $1"
      case "$1" in
        --output) OUTPUT=$2 ;; --release) ARTIFACT=$2 ;;
        --arch) ARCH=$2 ;;
      esac
      shift 2 ;;
    --skip-tests|--dry-run)
      [[ "$STAGE" == prepare ]] || die "$1 is only supported by prepare"
      PREPARE_FLAGS+=("$1"); shift ;;
    --results)
      shift
      while [[ $# -gt 0 && "$1" != --* ]]; do RESULTS+=("$1"); shift; done ;;
    -*) die "Unknown flag: $1" ;;
    *)
      [[ "$STAGE" == prepare && "$version_set" == false ]] || die "Unexpected argument: $1"
      VERSION_COMMAND=$1; version_set=true; shift ;;
  esac
done

umask 077
STAGE_TMP=$(mktemp -d)
BUILD_ACTIVE= ARTIFACT_PENDING=
cleanup_stage() {
  local status=$?
  if [[ -n "$BUILD_ACTIVE" ]]; then docker buildx rm --keep-state "$BUILD_ACTIVE" >/dev/null 2>&1 || true; fi
  if [[ -n "$ARTIFACT_PENDING" ]]; then
    rm -f "$ARTIFACT_PENDING/release.json" "$ARTIFACT_PENDING/release.bundle"
    rmdir "$ARTIFACT_PENDING" 2>/dev/null || true
  fi
  rm -rf "$STAGE_TMP"
  exit "$status"
}
trap cleanup_stage EXIT

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi | awk '{print $1}'
}
valid_digest() { [[ "$1" =~ ^sha256:[0-9a-f]{64}$ ]]; }
absolute_path() {
  local path=$1 parent name
  if [[ -d "$path" ]]; then (cd "$path"; pwd -P); return; fi
  if [[ -e "$path" || -L "$path" ]]; then realpath "$path"; return; fi
  parent=$(absolute_path "$(dirname "$path")")
  name=$(basename "$path")
  case "$name" in
    .) printf '%s\n' "$parent" ;; ..) dirname "$parent" ;;
    *) printf '%s/%s\n' "${parent%/}" "$name" ;;
  esac
}
source_git() { git -C "$SOURCE" "$@"; }
source_shell() { (cd "$SOURCE"; bash -euo pipefail -c 'source build/release-common.sh; '"$1" release "${@:2}"); }
settings() {
  source_shell 'version=$(current_version); jq -n --arg image "$IMAGE" --arg source_url "$SOURCE_URL" --arg version "$version" \
    --arg tags "$(release_image_tags "$version")" \
    '\''{image:$image,source_url:$source_url,version:$version,tags:($tags|split("\n"))}'\'
}
write_result() {
  mkdir -p "$(dirname "$OUTPUT")"
  local temporary
  temporary=$(mktemp "${OUTPUT}.XXXXXX")
  cat > "$temporary"
  # A hard link fails if the output already exists, including concurrent writers.
  if ! ln "$temporary" "$OUTPUT"; then rm -f "$temporary"; die 'Result already exists'; fi
  rm -f "$temporary"
}

prepare_stage() {
  [[ -n "$OUTPUT" ]] || die 'prepare requires --output'
  OUTPUT=$(absolute_path "$OUTPUT")
  [[ "$OUTPUT" != "$ROOT" && "$OUTPUT" != "$ROOT/"* ]] || die 'Prepared artifacts must be outside the source checkout'
  [[ ! -e "$OUTPUT" ]] || die 'Output already exists; retain it for retries or choose a new directory'
  [[ -z "$(git status --porcelain)" ]] || die 'Working tree is not clean. Commit or stash changes first.'
  [[ "$(git rev-parse --is-shallow-repository)" == false ]] || die 'Preparation requires a full Git checkout, including release tags'
  BRANCH=$(git symbolic-ref --short HEAD)
  BASE=$(git rev-parse HEAD)
  REMOTE=$(git remote get-url --push --all origin)
  [[ -n "$REMOTE" && "$REMOTE" != *$'\n'* && "$REMOTE" != *$'\r'* ]] || die 'Prepare requires one origin push URL'
  SOURCE="$STAGE_TMP/source"
  git clone --quiet --no-local --branch "$BRANCH" -- "$ROOT" "$SOURCE"
  [[ "$(source_git rev-parse HEAD)" == "$BASE" ]] || die 'Source branch changed during preparation'
  source_git config user.name "$(git config user.name)"
  source_git config user.email "$(git config user.email)"
  source_shell 'prepare_release "$@"' "$VERSION_COMMAND" ${PREPARE_FLAGS[@]+"${PREPARE_FLAGS[@]}"}
  [[ " ${PREPARE_FLAGS[*]:-} " != *' --dry-run '* ]] || return 0
  local config
  config=$(settings)
  VERSION=$(jq -r .version <<< "$config")
  TAG="v$VERSION"
  source_git bundle create "$STAGE_TMP/release.bundle" "refs/tags/$TAG"
  jq -n --argjson config "$config" --arg remote "$REMOTE" --arg branch "$BRANCH" --arg base "$BASE" \
    --arg sha "$(source_git rev-parse HEAD)" --arg tag "$TAG" \
    --arg tag_sha "$(source_git rev-parse "refs/tags/$TAG")" \
    --arg created "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg checksum "$(sha256 "$STAGE_TMP/release.bundle")" \
    '$config + {schema:2,remote:$remote,branch:$branch,base_sha:$base,source_sha:$sha,tag:$tag,tag_sha:$tag_sha,
      platforms:["linux/amd64","linux/arm64"],created:$created,bundle_sha256:$checksum}' > "$STAGE_TMP/release.json"
  mkdir -p "$(dirname "$OUTPUT")"
  mkdir "$OUTPUT"
  ARTIFACT_PENDING=$OUTPUT
  cp "$STAGE_TMP/release.bundle" "$STAGE_TMP/release.json" "$OUTPUT/"
  ARTIFACT_PENDING=
  printf 'Prepared %s: %s\nArtifact: %s\nNothing pushed.\n' "$TAG" "$(source_git rev-parse HEAD)" "$OUTPUT"
}

load_prepared() {
  [[ -n "$ARTIFACT" ]] || die 'Specify --release'
  ARTIFACT=$(cd "$ARTIFACT"; pwd -P)
  META="$ARTIFACT/release.json"
  jq -e '.schema == 2 and
    ([.remote,.image,.source_url] | all(type == "string" and length > 0 and (explode | all(. >= 32 and . != 127)))) and
    ([.source_sha,.base_sha,.tag_sha] | all(type == "string" and test("^[0-9a-f]{40}$"))) and
    (.version | type == "string" and test("^[0-9]+\\.[0-9]+\\.[0-9]+(-[A-Z]+[0-9]+)?$")) and
    .tag == ("v" + .version) and .platforms == ["linux/amd64","linux/arm64"] and
    (.created | strptime("%Y-%m-%dT%H:%M:%SZ") | length == 8)' "$META" >/dev/null || die 'Invalid release metadata'
  BRANCH=$(jq -r .branch "$META"); TAG=$(jq -r .tag "$META")
  VERSION=$(jq -r .version "$META"); REVISION=$(jq -r .source_sha "$META")
  git check-ref-format "refs/heads/$BRANCH"
  [[ "$(sha256 "$ARTIFACT/release.bundle")" == "$(jq -r .bundle_sha256 "$META")" ]] || die 'Release bundle checksum mismatch'
  SOURCE="$STAGE_TMP/source"
  git init --quiet "$SOURCE"
  source_git bundle verify "$ARTIFACT/release.bundle"
  source_git fetch --quiet "$ARTIFACT/release.bundle" "refs/tags/$TAG:refs/tags/$TAG"
  [[ "$(source_git cat-file -t "refs/tags/$TAG")" == tag &&
     "$(source_git rev-parse "refs/tags/$TAG")" == "$(jq -r .tag_sha "$META")" ]] || die 'Release annotated tag mismatch'
  source_git checkout --quiet -b "$BRANCH" "refs/tags/$TAG^{}"
  # Use the prepared destinations, regardless of this worker's checkout or defaults.
  source_git remote add origin "$(jq -r .remote "$META")"
  export FRONA_IMAGE=$(jq -r .image "$META") FRONA_SOURCE_URL=$(jq -r .source_url "$META")
  [[ "$(source_git rev-list --parents -n 1 HEAD)" == "$REVISION $(jq -r .base_sha "$META")" ]] || die 'Release commit or parent mismatch'
  local config
  config=$(settings)
  jq -e --argjson config "$config" '.image == $config.image and .source_url == $config.source_url and .version == $config.version and .tags == $config.tags' "$META" >/dev/null ||
    die 'Release image/version/tags disagree with the bundled source'
  [[ "$VERSION" == *-* || "$BRANCH" == main ]] || die 'Stable releases must originate from main'
  IMAGE=$(jq -r .image "$META")
}

build_stage() {
  [[ "$ARCH" == amd64 || "$ARCH" == arm64 ]] || die 'build requires --arch amd64 or arm64'
  [[ -n "$OUTPUT" && ! -e "$OUTPUT" ]] || die 'build requires a new --output file'
  OUTPUT=$(absolute_path "$OUTPUT")
  load_prepared
  local native digest
  native=$(docker info --format '{{.OSType}}/{{.Architecture}}')
  case "$native" in linux/x86_64) native=linux/amd64 ;; linux/aarch64) native=linux/arm64 ;; esac
  [[ "$native" == "linux/$ARCH" ]] || die "Expected native linux/$ARCH; found $native"
  export PLATFORM="$native" FRONA_BUILDER="frona-release-$ARCH"
  docker buildx create --name "$FRONA_BUILDER" --driver docker-container --platform "$native"
  BUILD_ACTIVE=$FRONA_BUILDER
  source_shell 'build_release_image "$@"' "$VERSION" "$REVISION" "$(jq -r .created "$META")" \
    -t "$IMAGE" --output type=image,push-by-digest=true,name-canonical=true,push=true \
    --metadata-file "$STAGE_TMP/build.json"
  digest=$(jq -er '."containerimage.digest"' "$STAGE_TMP/build.json")
  valid_digest "$digest" || die 'Buildx did not return an image digest'
  jq --arg platform "$native" --arg digest "$digest" \
    '{schema:2,source_sha,tag_sha,image,platform:$platform,digest:$digest}' "$META" | write_result
  printf 'Built %s: %s\nResult: %s\n' "$native" "$digest" "$OUTPUT"
}

inspect_image() {
  local reference=$1 output=$2 missing_ok=${3:-false}
  if docker buildx imagetools inspect --raw "$reference" > "$output" 2> "$STAGE_TMP/inspect-error"; then
    jq -e 'type == "object"' "$output" >/dev/null
  elif [[ "$missing_ok" == true ]] && {
    grep -Fq "$reference: not found" "$STAGE_TMP/inspect-error" || grep -Eiq 'manifest unknown|manifest_unknown' "$STAGE_TMP/inspect-error";
  }; then
    printf 'null\n' > "$output"
  else
    cat "$STAGE_TMP/inspect-error" >&2
    die 'Cannot inspect registry image'
  fi
}
validate_image() {
  jq -e --arg revision "$REVISION" --arg version "$VERSION" --argjson platforms "$2" '
    [.manifests[] | select((.annotations["vnd.docker.reference.type"] == "attestation-manifest" and
      .platform == {os:"unknown",architecture:"unknown"}) | not) |
      (.platform.os + "/" + .platform.architecture)] as $actual |
    ($actual | sort) == ($platforms | sort) and
    .annotations["org.opencontainers.image.revision"] == $revision and
    .annotations["org.opencontainers.image.version"] == $version' "$1" >/dev/null || die 'Image platform, revision, or version mismatch'
}
published_digest() {
  local digest
  digest=$(docker buildx imagetools inspect "$1" --format '{{json .Manifest.Digest}}' | jq -er .)
  valid_digest "$digest" || die 'Registry did not return a valid digest'
  printf '%s\n' "$digest"
}
remote_state() {
  local branch_sha tag_sha peeled ref value
  branch_sha= tag_sha= peeled=
  source_git ls-remote origin "refs/heads/$BRANCH" "refs/tags/$TAG" "refs/tags/$TAG^{}" > "$STAGE_TMP/remote-refs"
  while read -r value ref; do
    case "$ref" in
      "refs/heads/$BRANCH") branch_sha=$value ;; "refs/tags/$TAG") tag_sha=$value ;;
      "refs/tags/$TAG^{}") peeled=$value ;;
    esac
  done < "$STAGE_TMP/remote-refs"
  if [[ "$tag_sha" == "$(jq -r .tag_sha "$META")" && "$peeled" == "$REVISION" ]]; then
    source_git fetch --quiet origin "refs/heads/$BRANCH" || die 'Cannot verify completed release branch'
    source_git merge-base --is-ancestor "$REVISION" FETCH_HEAD || die 'Released commit is no longer on the remote branch'
    printf 'complete\n'
  else
    [[ -z "$tag_sha" ]] || die 'Remote release tag already exists with different contents'
    [[ "$branch_sha" == "$(jq -r .base_sha "$META")" ]] || die 'Remote source branch moved; prepare a fresh release'
    printf 'pending\n'
  fi
}

push_stage() {
  [[ ${#RESULTS[@]} == 2 ]] || die 'Push requires successful AMD64 and ARM64 build results'
  load_prepared
  local state arch digest raw expected version_ref tag annotation
  state=$(remote_state)
  jq -se --slurpfile m "$META" '
    length == 2 and all(.schema == 2 and .source_sha == $m[0].source_sha and
      .tag_sha == $m[0].tag_sha and .image == $m[0].image and
      (.digest | type == "string" and test("^sha256:[0-9a-f]{64}$"))) and
    (map(.platform)|sort) == ["linux/amd64","linux/arm64"] and (map(.digest)|unique|length) == 2' "${RESULTS[@]}" >/dev/null ||
    die 'Missing, duplicate, or mismatched build results'
  local inputs=() options=()
  for arch in amd64 arm64; do
    digest=$(jq -sr --arg platform "linux/$arch" '.[] | select(.platform == $platform) | .digest' "${RESULTS[@]}")
    inspect_image "$IMAGE@$digest" "$STAGE_TMP/$arch.json"
    validate_image "$STAGE_TMP/$arch.json" "[\"linux/$arch\"]"
    inputs+=("$IMAGE@$digest")
  done
  while IFS= read -r annotation; do options+=(--annotation "$annotation"); done < <(
    jq -r '.annotations | to_entries[] | select(.key|startswith("org.opencontainers.image.")) |
      select(.value|type == "string") | "index:" + .key + "=" + .value' "$STAGE_TMP/amd64.json")
  raw=$(docker buildx imagetools create --dry-run "${options[@]}" "${inputs[@]}")
  printf '%s' "$raw" > "$STAGE_TMP/candidate.json"
  validate_image "$STAGE_TMP/candidate.json" '["linux/amd64","linux/arm64"]'
  expected="sha256:$(sha256 "$STAGE_TMP/candidate.json")"
  version_ref="$IMAGE:$TAG"
  inspect_image "$version_ref" "$STAGE_TMP/previous.json" true
  if [[ "$(cat "$STAGE_TMP/previous.json")" != null ]]; then
    validate_image "$STAGE_TMP/previous.json" '["linux/amd64","linux/arm64"]'
    [[ "$(published_digest "$version_ref")" == "$expected" ]] || die 'Version image already exists with different contents; reuse the original build results'
  elif [[ "$state" == complete ]]; then
    die 'Git release is complete but its version image is missing; manual recovery required'
  fi
  if [[ "$state" == complete ]]; then echo "$TAG is already released; image tags left unchanged."; return 0; fi
  [[ "$(remote_state)" == pending ]] || die 'Remote release state changed during publication checks; retry'
  while IFS= read -r tag; do
    docker buildx imagetools create --tag "$IMAGE:$tag" "${options[@]}" "${inputs[@]}"
    [[ "$(published_digest "$IMAGE:$tag")" == "$expected" ]] || die 'Published tag does not match the verified image index'
  done < <(jq -r '.tags[]' "$META")
  source_shell 'push_release_git "$1"' "$TAG"
  printf 'Released %s (%s)\n' "$TAG" "$REVISION"
}

case "$STAGE" in
  prepare) prepare_stage ;; build) build_stage ;; push) push_stage ;;
  *) die "Unknown stage: $STAGE" ;;
esac
