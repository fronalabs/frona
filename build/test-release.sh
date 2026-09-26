#!/usr/bin/env bash
# Integration tests: real temporary Git repos; fake Cargo and Docker.
set -euo pipefail

hash() { if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi | awk '{print $1}'; }
fake_docker() {
  local command=$1; shift
  [[ "$command" != info ]] || { echo "${NATIVE_PLATFORM:-linux/amd64}"; return; }
  local action=$1; shift
  case "$action" in
    create|inspect|use|rm) return ;;
    build)
      [[ -z "${FAIL_BUILD:-}" ]] || return 1
      local platform= metadata= tag= annotation annotations='{}' source_label=
      while [[ $# -gt 0 ]]; do
        case "$1" in
          --platform) platform=$2; shift ;; --metadata-file) metadata=$2; shift ;;
          --label)
            [[ "$2" != org.opencontainers.image.source=* ]] || source_label=${2#*=}
            shift ;;
          -t) tag=$2; shift ;; --annotation)
            annotation=${2#index:}; shift
            annotations=$(jq -c --arg key "${annotation%%=*}" --arg value "${annotation#*=}" '. + {($key):$value}' <<< "$annotations") ;;
        esac
        shift
      done
      jq -nc --arg platform "$platform" --arg source_label "$source_label" --argjson annotations "$annotations" \
        '{platform:$platform,source_label:$source_label,source_annotation:$annotations["org.opencontainers.image.source"]}' >> "$IMAGE_META_LOG"
      local digest manifest
      digest="sha256:$(printf '%s' "$platform" | hash)"
      manifest=$(jq -n --arg platforms "$platform" --arg digest "$digest" --argjson annotations "$annotations" \
        '{schemaVersion:2,annotations:$annotations,manifests:($platforms|split(",")|map(split("/")|
          {platform:{os:.[0],architecture:.[1]},digest:$digest}))}')
      [[ "$(jq -r '.annotations["org.opencontainers.image.revision"]' <<< "$manifest")" == "$(git rev-parse HEAD)" ]]
      if [[ -n "$metadata" ]]; then
        [[ -n "${MISSING_METADATA:-}" ]] || jq -n --arg digest "$digest" '{"containerimage.digest":$digest}' > "$metadata"
        tag="$tag@$digest"
      fi
      jq --arg tag "$tag" --argjson manifest "$manifest" '. + {($tag):$manifest}' "$REGISTRY" > "$REGISTRY.tmp"
      mv "$REGISTRY.tmp" "$REGISTRY" ;;
    imagetools)
      action=$1; shift
      if [[ "$action" == inspect ]]; then
        [[ -z "${FAIL_INSPECT:-}" ]] || { echo '401 Unauthorized' >&2; return 1; }
        local reference raw
        if [[ "$1" == --raw ]]; then reference=$2; else reference=$1; fi
        jq -e --arg ref "$reference" 'has($ref)' "$REGISTRY" >/dev/null || { echo "$reference: not found" >&2; return 1; }
        raw=$(jq -S --arg ref "$reference" '.[$ref]' "$REGISTRY")
        if [[ "$1" == --raw ]]; then printf '%s\n' "$raw"; else
          printf '%s' "$raw" | hash | jq -R '"sha256:" + .'
        fi
      else
        local dry=false tag= annotation annotations='{}' manifests='[]' manifest
        while [[ $# -gt 0 ]]; do
          case "$1" in
            --dry-run) dry=true ;; --tag) tag=$2; shift ;;
            --annotation)
              annotation=${2#index:}; shift
              annotations=$(jq -c --arg key "${annotation%%=*}" --arg value "${annotation#*=}" '. + {($key):$value}' <<< "$annotations") ;;
            *) manifests=$(jq -c --arg ref "$1" --argjson prior "$manifests" '$prior + .[$ref].manifests' "$REGISTRY") ;;
          esac
          shift
        done
        manifest=$(jq -nS --argjson annotations "$annotations" --argjson manifests "$manifests" \
          '{schemaVersion:2,annotations:$annotations,manifests:$manifests}')
        if [[ "$dry" == true ]]; then printf '%s\n' "$manifest"; else
          [[ -z "${FAIL_PUBLISH:-}" ]] || return 1
          [[ -z "${FAIL_LATEST:-}" || "$tag" != *:latest ]] || return 1
          jq --arg tag "$tag" --argjson manifest "$manifest" '. + {($tag):$manifest}' "$REGISTRY" > "$REGISTRY.tmp"
          mv "$REGISTRY.tmp" "$REGISTRY"
        fi
      fi ;;
    *) return 1 ;;
  esac
}

case "$(basename "$0")" in
  cargo)
    printf 'cargo %s\n' "$*" >> "$TOOL_LOG"
    case "$1" in
      test) [[ -z "${FAIL_TESTS:-}" ]] ;;
      update) [[ -z "${FAIL_LOCK:-}" ]] && cp Cargo.toml Cargo.lock ;;
    esac
    exit ;;
  docker) printf 'docker %s\n' "$*" >> "$TOOL_LOG"; fake_docker "$@"; exit ;;
esac

ROOT=$(cd "$(dirname "$0")/.."; pwd)
TEST_ROOT=$(mktemp -d)
cleanup() {
  local status=$?
  if [[ "$status" != 0 && -f "${CASE:-}/output" ]]; then tail -n 30 "$CASE/output" >&2; fi
  rm -rf "$TEST_ROOT"
  exit "$status"
}
trap cleanup EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset FRONA_IMAGE FRONA_SOURCE_URL PLATFORM FRONA_BUILDER GIT_DIR GIT_WORK_TREE
REAL_GIT=$(command -v git); export REAL_GIT
mkdir "$TEST_ROOT/bin"
for tool in cargo docker; do cp "$0" "$TEST_ROOT/bin/$tool"; chmod +x "$TEST_ROOT/bin/$tool"; done
export PATH="$TEST_ROOT/bin:$PATH"
count=0
setup() {
  count=$((count + 1))
  CASE="$TEST_ROOT/$count"; mkdir "$CASE"
  export TOOL_LOG="$CASE/tools.log" REGISTRY="$CASE/registry.json"
  export IMAGE_META_LOG="$CASE/image-metadata.jsonl"
  : > "$IMAGE_META_LOG"
  : > "$TOOL_LOG"; echo '{}' > "$REGISTRY"
  SOURCE="$CASE/source"; REMOTE="$CASE/remote.git"; ARTIFACT="$CASE/release"
  git init --bare -q "$REMOTE"
  mkdir -p "$SOURCE/build" "$SOURCE/web"
  cp "$ROOT"/build/{common.sh,release.sh,release-common.sh,release-stages.sh} "$SOURCE/build/"
  printf '[workspace.package]\nversion = "2026.9.0"\n' > "$SOURCE/Cargo.toml"
  cp "$SOURCE/Cargo.toml" "$SOURCE/Cargo.lock"
  printf '{"version":"2026.9.0"}\n' > "$SOURCE/web/package.json"
  cp "$SOURCE/web/package.json" "$SOURCE/web/package-lock.json"
  cd "$SOURCE"
  git init -q; git checkout -qb main
  git config user.name 'Release Test'; git config user.email test@example.invalid
  git add .; git commit -qm initial
  BASE=$(git rev-parse HEAD)
  git remote add origin "$REMOTE"; git push -q origin main
  cat > "$REMOTE/hooks/pre-receive" <<'SH'
#!/usr/bin/env bash
[[ -z "${FAIL_GIT_PUSH:-}" ]] || exit 1
echo git-push >> "$TOOL_LOG"
SH
  chmod +x "$REMOTE/hooks/pre-receive"
}
release() { bash build/release.sh "$@" >> "$CASE/output" 2>&1; }
reject() {
  local status=0
  # Execute a separate shell so an expected failure cannot disable errexit inside helpers.
  "$@" >> "$CASE/output" 2>&1 || status=$?
  [[ "$status" != 0 ]] || { echo "Unexpected success: $*" >&2; exit 1; }
}
prepare() { release prepare "${1:-2026.9.1}" --output "$ARTIFACT"; }
builds() {
  NATIVE_PLATFORM=linux/amd64 release build --release "$ARTIFACT" --arch amd64 --output "$CASE/amd64.json"
  NATIVE_PLATFORM=linux/arm64 release build --release "$ARTIFACT" --arch arm64 --output "$CASE/arm64.json"
}
push_release() { release push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/arm64.json"; }
unchanged() { [[ "$(git --git-dir="$REMOTE" rev-parse main)" == "$BASE" ]]; }
untagged() { jq -e 'keys|all(contains("@sha256:"))' "$REGISTRY" >/dev/null; }
pass() { printf 'ok %s - %s\n' "$count" "$1"; }

setup
release 2026.9.1
[[ "$(git rev-parse HEAD)" == "$(git --git-dir="$REMOTE" rev-parse main)" ]]
awk '/docker buildx build/ { built=1 } /git-push/ { if (!built) exit 1; pushed=1 } END { if (!pushed) exit 1 }' "$TOOL_LOG"
pass 'local release builds before pushing Git'

setup
FAIL_BUILD=1 reject bash build/release.sh 2026.9.1
unchanged; [[ "$(git rev-parse HEAD)" == "$BASE" && -z "$(git tag)" ]]
pass 'local image failure rolls back local refs'

setup
release 2026.9.1 --dry-run
[[ ! -s "$TOOL_LOG" ]]
release 2026.9.1 --skip-tests --skip-docker
! grep -Eq '^docker|^cargo test' "$TOOL_LOG"
pass 'existing dry-run and skip flags remain available'

setup
release prepare 2026.9.1 --output "$ARTIFACT" --dry-run
[[ ! -e "$ARTIFACT" && ! -s "$TOOL_LOG" ]]
prepare
cp "$TOOL_LOG" "$CASE/before-build.log"
for flag in --dry-run --skip-tests; do
  reject bash build/release.sh build --release "$ARTIFACT" --arch amd64 --output "$CASE/amd64.json" "$flag"
done
[[ ! -e "$CASE/amd64.json" ]]
cmp "$TOOL_LOG" "$CASE/before-build.log"
builds
cp "$REGISTRY" "$CASE/before-push.json"
cp "$TOOL_LOG" "$CASE/before-push.log"
for flag in --dry-run --skip-tests; do
  reject bash build/release.sh push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/arm64.json" "$flag"
  reject bash build/release.sh check "$flag"
done
cmp "$REGISTRY" "$CASE/before-push.json"
cmp "$TOOL_LOG" "$CASE/before-push.log"
unchanged; [[ -z "$(git --git-dir="$REMOTE" tag)" ]]
pass 'prepare flags are rejected by other stages before any side effects'

setup
FAIL_TESTS=1 reject bash build/release.sh prepare 2026.9.1 --output "$ARTIFACT"
FAIL_LOCK=1 reject bash build/release.sh prepare 2026.9.1 --output "$ARTIFACT"
[[ ! -e "$ARTIFACT" && -z "$(git status --porcelain)" ]]; unchanged
pass 'failed preparation leaves checkout and remote unchanged'

setup
prepare
[[ "$(git rev-parse HEAD)" == "$BASE" && -z "$(git tag)" ]]
reject bash build/release.sh prepare 2026.9.1 --output "$ARTIFACT"
builds; untagged; unchanged
push_release
[[ "$(git --git-dir="$REMOTE" rev-parse main)" == "$(jq -r .source_sha "$ARTIFACT/release.json")" ]]
jq '."ghcr.io/fronalabs/frona:latest"={newer:true}' "$REGISTRY" > "$REGISTRY.tmp"; mv "$REGISTRY.tmp" "$REGISTRY"
push_release
jq -e '."ghcr.io/fronalabs/frona:latest".newer' "$REGISTRY" >/dev/null
pass 'stages publish exact prepared refs; completed retry preserves newer latest'

setup
prepare 2026.9.1-RC1; builds; push_release
jq -e 'has("ghcr.io/fronalabs/frona:latest")|not' "$REGISTRY" >/dev/null
pass 'prereleases never publish latest'

setup
prepare
FAIL_BUILD=1 reject bash build/release.sh build --release "$ARTIFACT" --arch amd64 --output "$CASE/amd64.json"
MISSING_METADATA=1 reject bash build/release.sh build --release "$ARTIFACT" --arch amd64 --output "$CASE/amd64.json"
NATIVE_PLATFORM=linux/arm64 reject bash build/release.sh build --release "$ARTIFACT" --arch amd64 --output "$CASE/amd64.json"
[[ ! -e "$CASE/amd64.json" ]]; unchanged
printf corrupt >> "$ARTIFACT/release.bundle"
reject bash build/release.sh build --release "$ARTIFACT" --arch amd64 --output "$CASE/amd64.json"
pass 'failed builds, wrong workers, and corrupt artifacts produce no result'

setup
prepare; builds
reject bash build/release.sh push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/amd64.json"
FAIL_INSPECT=1 reject bash build/release.sh push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/arm64.json"
untagged; unchanged
FAIL_LATEST=1 reject bash build/release.sh push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/arm64.json"
unchanged
FAIL_GIT_PUSH=1 reject bash build/release.sh push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/arm64.json"
unchanged; [[ -z "$(git --git-dir="$REMOTE" tag)" ]]
push_release
pass 'publication and atomic Git failures retry without another version bump'

setup
prepare; builds
git commit --allow-empty -qm concurrent; git push -q origin main
reject bash build/release.sh push --release "$ARTIFACT" --results "$CASE/amd64.json" "$CASE/arm64.json"
untagged
pass 'moved branch blocks named publication'

setup
# Exercise a fork with actual Git pushes through a local SSH transport.
cat > "$CASE/ssh" <<'SH'
#!/usr/bin/env bash
case "${*: -1}" in
  *git-upload-pack*) exec git-upload-pack "$TEST_FORK_REMOTE" ;;
  *git-receive-pack*) exec git-receive-pack "$TEST_FORK_REMOTE" ;;
  *) exit 1 ;;
esac
SH
chmod +x "$CASE/ssh"
export GIT_SSH_COMMAND="$CASE/ssh" GIT_SSH_VARIANT=ssh TEST_FORK_REMOTE="$REMOTE"
git remote set-url origin git@forge.example.test:team/frona.git
export FRONA_IMAGE=registry.example.test/team/custom-frona FRONA_SOURCE_URL=https://forge.example.test/team/frona
prepare
# Worker checkouts and environment settings cannot redirect a prepared release.
git remote set-url origin /unused/worker/origin
export FRONA_IMAGE=registry.example.test/wrong/image FRONA_SOURCE_URL=https://wrong.example.test/repo
builds
jq -se 'length == 2 and (map(.platform) | sort) == ["linux/amd64","linux/arm64"] and
  all(.source_label == "https://forge.example.test/team/frona" and .source_annotation == .source_label)' "$IMAGE_META_LOG" >/dev/null
[[ "$(jq -r .image "$ARTIFACT/release.json")" == registry.example.test/team/custom-frona ]]
push_release
[[ "$(git --git-dir="$REMOTE" rev-parse main)" == "$(jq -r .source_sha "$ARTIFACT/release.json")" ]]
jq -e 'keys | all(startswith("registry.example.test/team/custom-frona"))' "$REGISTRY" >/dev/null
jq -e '.[] | .annotations["org.opencontainers.image.source"] == "https://forge.example.test/team/frona"' "$REGISTRY" >/dev/null
unset GIT_SSH_COMMAND GIT_SSH_VARIANT TEST_FORK_REMOTE FRONA_IMAGE FRONA_SOURCE_URL
pass 'explicit fork targets survive different worker origins and environment settings'

printf 'All release script checks passed.\n'
