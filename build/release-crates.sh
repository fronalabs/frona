#!/usr/bin/env bash
# Prepare in isolation; retain a journal once publication can have side effects.
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
die() { echo "$*" >&2; exit 1; }
level=auto
level_set=false
dry_run=false
for argument in "$@"; do
  case "$argument" in
    auto|patch|minor|major)
      [[ "$level_set" == false ]] || die 'Specify one version level'
      level=$argument; level_set=true ;;
    --dry-run) dry_run=true ;;
    --help|-h)
      echo 'Usage: mise run release:crates -- [auto|patch|minor|major] [--dry-run]'
      echo 'Detect, verify, commit, publish and tag changed libraries. Dry runs only prepare and verify.'
      exit 0 ;;
    *) die "Unknown argument: $argument" ;;
  esac
done
for tool in cargo release-plz cargo-semver-checks jq curl git flock; do
  command -v "$tool" >/dev/null || die "Missing $tool; run mise install"
done
[[ -z $(git status --porcelain) ]] || die 'Commit or stash changes before releasing libraries (including dry runs).'
branch=$(git symbolic-ref --short HEAD) || die 'Check out a branch first'
[[ "$dry_run" == true || "$branch" == main ]] || die 'Publish library releases from main'
base=$(git rev-parse HEAD)
state=$(git rev-parse --path-format=absolute --git-path frona-crates-release)
exec 9>"${state}.lock"
flock -n 9 || die 'Another library release is running'
packages=(frona-text frona-derive frona-model-catalog frona-harness frona-pkm frona-sandbox)
package_json=$(printf '%s\n' "${packages[@]}" | jq -R . | jq -s .)
temporary=$(mktemp -d)
staging=
cleanup() {
  local status=$?
  rm -rf "$temporary"
  if [[ -n "$staging" ]]; then rm -rf "$staging"; fi
  if [[ "$status" -ne 0 && -f "$state/plan.json" ]]; then
    echo 'Release saved. Rerun mise run release:crates to resume without another version bump.' >&2
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"$root/target"}

metadata() {
  cargo metadata --no-deps --format-version 1 | jq --argjson names "$package_json" \
    '[.packages[] | select(.name as $name | $names | index($name)) | {name, version}]'
}
# Only a real 404 means unpublished. Authentication, network and registry errors
# must never be interpreted as permission to republish an existing version.
registry() {
  local name=$1 status
  status=$(curl --silent --show-error --retry 3 --connect-timeout 15 --max-time 120 \
    --output "$temporary/index" --write-out '%{http_code}' \
    "https://index.crates.io/${name:0:2}/${name:2:2}/$name")
  case "$status" in
    200) jq -s '[.[].vers]' "$temporary/index" ;;
    404) echo '[]' ;;
    *) die "Registry lookup failed for $name (HTTP $status)" ;;
  esac
}
unpublished() {
  local name=$1 version=$2
  jq -e --arg version "$version" 'index($version) == null' "$temporary/$name.json" >/dev/null
}
refresh_registry() {
  local name
  for name in "${packages[@]}"; do registry "$name" > "$temporary/$name.json"; done
}
select_packages() {
  local name version
  while IFS=$'\t' read -r name version; do
    if unpublished "$name" "$version"; then
      jq -n --arg name "$name" --arg version "$version" '{name: $name, version: $version}'
    fi
  done < <(jq -r '.[] | [.name, .version] | @tsv' "$1")
}

if [[ -f "$state/plan.json" ]]; then
  plan="$state/plan.json"
  commit=$(jq -r .commit "$plan")
  [[ "$base" == "$commit" || "$base" == "$(jq -r .base "$plan")" ]] || \
    die "An unfinished release exists at $state; return to its base or release commit before resuming."
  [[ "$branch" == "$(jq -r .branch "$plan")" ]] || die 'Resume on the original release branch'
  [[ "$level" == auto ]] || die 'A release is pending; rerun without a version override to resume it'
  echo "Resuming prepared release $commit"
  work="$state/work"
else
  work="$temporary/work"
  git clone --quiet --shared --no-hardlinks "$root" "$work"
  git -C "$work" checkout --quiet --detach "$base"
  # Use the project's real URL for release-plz links, not the temporary clone path.
  git -C "$work" remote set-url origin "$(git remote get-url origin)"
  git -C "$work" config user.name "$(git var GIT_AUTHOR_IDENT | sed 's/ <.*//')"
  git -C "$work" config user.email "$(git var GIT_AUTHOR_IDENT | sed 's/.*<\(.*\)>.*/\1/')"
  cd "$work"
  metadata > "$temporary/before.json"
  refresh_registry
  release-plz update --config release-plz.toml --no-changelog
  metadata > "$temporary/auto.json"
  select_packages "$temporary/auto.json" | jq -s . > "$temporary/selected.json"
  if [[ "$level" != auto ]]; then
    # Bump from the committed version, not from release-plz's proposed version.
    # Only this disposable clone's generated edits are discarded.
    git restore --worktree -- .
    while IFS=$'\t' read -r name previous; do
      if ! unpublished "$name" "$previous" && \
        jq -e --arg name "$name" 'any(.[]; .name == $name)' "$temporary/selected.json" >/dev/null; then
        cargo set-version -p "$name" --bump "$level"
      fi
    done < <(jq -r '.[] | [.name, .version] | @tsv' "$temporary/before.json")
    # Account for dependency-only releases introduced by the override as well.
    release-plz update --config release-plz.toml --no-changelog --allow-dirty
  fi
  metadata > "$temporary/after.json"
  select_packages "$temporary/after.json" | jq -s . > "$temporary/selected.json"
  [[ $(jq length "$temporary/selected.json") -gt 0 ]] || { echo 'No libraries need a release.'; exit 0; }
  echo 'Selected library versions (release-plz reports change reasons above):'
  jq -r '.[] | "  \(.name) \(.version)"' "$temporary/selected.json"
  host=$(rustc -vV | sed -n 's/^host: //p')
  [[ -n "$host" ]] || die 'Cannot determine the Rust host target'
  for manifest in crates/frona-{harness,sandbox}/tests/fixtures/*/Cargo.toml; do
    cargo metadata --format-version 1 --filter-platform "$host" --manifest-path "$manifest" >/dev/null
  done
  args=()
  while IFS= read -r name; do args+=(-p "$name"); done < <(jq -r '.[].name' "$temporary/selected.json")
  cargo test "${args[@]}"
  cargo publish --dry-run --allow-dirty "${args[@]}"
  if [[ "$dry_run" == true ]]; then
    echo 'Dry run passed. No checkout changes, commits, tags, uploads or pushes.'
    exit 0
  fi
  git add -u
  git commit --allow-empty -m 'release: libraries'
  commit=$(git rev-parse HEAD)
  jq -n --arg base "$base" --arg commit "$commit" --arg branch "$branch" \
    --slurpfile packages "$temporary/selected.json" \
    '{base: $base, commit: $commit, branch: $branch, packages: $packages[0]}' > "$temporary/plan.json"
  # Publish the journal atomically before touching the original checkout or registry.
  staging=$(mktemp -d "${state}.preparing.XXXXXX")
  mv "$work" "$staging/work"
  mv "$temporary/plan.json" "$staging/plan.json"
  mv "$staging" "$state"
  work="$state/work"
  plan="$state/plan.json"
fi

cd "$work"
refresh_registry
args=()
while IFS=$'\t' read -r name version; do
  if unpublished "$name" "$version"; then args+=(-p "$name"); fi
done < <(jq -r '.packages[] | [.name, .version] | @tsv' "$plan")
if [[ "$dry_run" == true ]]; then
  if [[ ${#args[@]} -gt 0 ]]; then cargo publish --dry-run "${args[@]}"; fi
  echo 'Pending release verified; no publication or Git changes.'
  exit 0
fi
cd "$root"
# Check existing tags before any upload. Never overwrite another release's tag.
tags=()
while IFS=$'\t' read -r name version; do
  tag="$name-v$version"
  if git rev-parse --verify "refs/tags/$tag" >/dev/null 2>&1; then
    [[ $(git rev-parse "$tag^{commit}") == "$commit" ]] || die "Conflicting tag: $tag"
  fi
  remote_tags=$(git ls-remote origin "refs/tags/$tag" "refs/tags/$tag^{}")
  if [[ -n "$remote_tags" ]]; then
    remote_commit=$(printf '%s\n' "$remote_tags" | tail -1 | cut -f1)
    [[ "$remote_commit" == "$commit" ]] || die "Conflicting remote tag: $tag"
    # Preserve the remote tag object if recovering with tags missing locally.
    git fetch --quiet origin "refs/tags/$tag:refs/tags/$tag"
  fi
  tags+=("refs/tags/$tag")
done < <(jq -r '.packages[] | [.name, .version] | @tsv' "$plan")
if [[ $(git rev-parse HEAD) != "$commit" ]]; then
  git fetch --quiet --no-tags "$work" "$commit"
  git merge --ff-only FETCH_HEAD
fi
git push --dry-run origin "HEAD:refs/heads/$branch"
if [[ ${#args[@]} -gt 0 ]]; then cargo publish "${args[@]}"; fi
for ref in "${tags[@]}"; do
  if ! git rev-parse --verify "$ref" >/dev/null 2>&1; then
    git tag -a "${ref#refs/tags/}" "$commit" -m "Release ${ref#refs/tags/}"
  fi
done
git push --atomic origin "HEAD:refs/heads/$branch" "${tags[@]}"
rm -rf "$state"
echo 'Library release completed.'
