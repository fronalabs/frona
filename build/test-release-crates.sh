#!/usr/bin/env bash
# Exercise release side effects using real Git repositories and fake Cargo/registry.
set -euo pipefail
source_dir=$(cd "$(dirname "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/bin"
export RELEASE_TEST_STATE="$stage/service"
mkdir "$RELEASE_TEST_STATE"
cat > "$stage/bin/curl" <<'CURL'
#!/usr/bin/env bash
set -euo pipefail
while [[ "$1" != --output ]]; do shift; done
output=$2
name=${!#}; name=${name##*/}
if [[ ${REGISTRY_ERROR:-false} == true ]]; then echo 503; exit; fi
if [[ ! -s "$RELEASE_TEST_STATE/$name" ]]; then echo 404; exit; fi
jq -Rn 'inputs | {vers: .}' < "$RELEASE_TEST_STATE/$name" > "$output"
echo 200
CURL
cat > "$stage/bin/rustc" <<'RUSTC'
#!/usr/bin/env bash
echo 'host: x86_64-unknown-linux-gnu'
RUSTC
cat > "$stage/bin/cargo-semver-checks" <<'CHECK'
#!/usr/bin/env bash
exit 0
CHECK
cat > "$stage/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$RELEASE_TEST_STATE/calls"
case "$1" in
  metadata)
    if [[ " $* " == *' --manifest-path '* ]]; then
      while [[ "$1" != --manifest-path ]]; do shift; done
      echo refreshed > "$(dirname "$2")/Cargo.lock"
    else
      for file in crates/*/Cargo.toml; do
        name=$(sed -n 's/^name = "\(.*\)"/\1/p' "$file")
        version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$file")
        jq -n --arg name "$name" --arg version "$version" '{name: $name, version: $version}'
      done | jq -s '{packages: .}'
    fi ;;
  set-version)
    name=$3; version=$4
    if [[ "$version" == --bump ]]; then
      current=$(sed -n 's/^version = "\(.*\)"/\1/p' "crates/$name/Cargo.toml")
      IFS=. read -r major minor patch <<< "$current"
      case "$5" in
        patch) version="$major.$minor.$((patch + 1))" ;;
        minor) version="$major.$((minor + 1)).0" ;;
        major) version="$((major + 1)).0.0" ;;
      esac
    fi
    sed -i "s/^version = .*/version = \"$version\"/" "crates/$name/Cargo.toml"
    echo updated > Cargo.lock ;;
  test) [[ ${TEST_FAILURE:-false} != true ]] ;;
  publish)
    [[ " $* " != *' --no-verify '* ]]
    if [[ " $* " == *' --dry-run '* ]]; then
      [[ ${VERIFY_FAILURE:-false} != true ]]
    else
      shift
      while [[ $# -gt 0 ]]; do
        [[ "$1" == -p ]]; name=$2; shift 2
        version=$(sed -n 's/^version = "\(.*\)"/\1/p' "crates/$name/Cargo.toml")
        if grep -Fxq "$version" "$RELEASE_TEST_STATE/$name" 2>/dev/null; then exit 42; fi
        echo "$version" >> "$RELEASE_TEST_STATE/$name"
        echo "$name@$version" >> "$RELEASE_TEST_STATE/uploads"
        if [[ ${PUBLISH_FAILURE:-false} == true ]]; then exit 1; fi
      done
    fi ;;
  *) exit 1 ;;
esac
CARGO
cat > "$stage/bin/release-plz" <<'PLZ'
#!/usr/bin/env bash
set -euo pipefail
echo "$*" >> "$RELEASE_TEST_STATE/updates"
[[ "$1" == update ]]
if [[ ${CHANGE:-none} == patch ]]; then
  for name in frona-text frona-harness frona-pkm; do
    if grep -q 'version = "0.1.0"' "crates/$name/Cargo.toml"; then
      cargo set-version -p "$name" 0.1.1
    fi
  done
fi
PLZ
chmod +x "$stage/bin/"*
export PATH="$stage/bin:$PATH"
fixture() {
  unset REGISTRY_ERROR TEST_FAILURE VERIFY_FAILURE PUBLISH_FAILURE CHANGE
  rm -rf "$stage/repo" "$stage/remote.git"
  rm -f "$RELEASE_TEST_STATE/"*
  mkdir -p "$stage/repo/build"
  cd "$stage/repo"
  cp "$source_dir/build/release-crates.sh" build/
  cp "$source_dir/release-plz.toml" .
  git init -q -b main
  git config user.name 'Release test'
  git config user.email 'release-test@example.invalid'
  echo 'application version unchanged' > Cargo.toml
  echo original > Cargo.lock
  for name in frona-text frona-derive frona-model-catalog frona-harness frona-pkm; do
    mkdir -p "crates/$name"
    printf 'name = "%s"\nversion = "0.1.0"\n' "$name" > "crates/$name/Cargo.toml"
    if [[ ${1:-published} == published ]]; then echo 0.1.0 > "$RELEASE_TEST_STATE/$name"; fi
  done
  mkdir -p crates/frona-harness/tests/fixtures/standalone
  echo fixture > crates/frona-harness/tests/fixtures/standalone/Cargo.toml
  echo original > crates/frona-harness/tests/fixtures/standalone/Cargo.lock
  git add .; git commit -qm initial
  git init -q --bare "$stage/remote.git"
  git remote add origin "$stage/remote.git"
  git push -q origin main
  initial=$(git rev-parse HEAD)
}
run() { bash build/release-crates.sh "$@" > "$stage/output" 2>&1; }
unchanged() {
  [[ $(git rev-parse HEAD) == "$initial" && -z $(git status --porcelain) ]]
  [[ -z $(git tag) ]]
  [[ ! -e "$RELEASE_TEST_STATE/uploads" ]]
}

fixture
run
unchanged
[[ ! -d .git/frona-crates-release ]]
echo 'ok - unchanged published crates are a no-op'

fixture new
run --dry-run
unchanged
grep -q '^publish --dry-run --allow-dirty .*frona-text' "$RELEASE_TEST_STATE/calls"
[[ ! -d .git/frona-crates-release ]]
echo 'ok - first-release dry run verifies packages without changing Git or publishing'

fixture
export CHANGE=patch
run
[[ $(git rev-list --count "$initial..HEAD") == 1 ]]
[[ $(git tag | wc -l) == 3 ]]
[[ $(wc -l < "$RELEASE_TEST_STATE/uploads") == 3 ]]
[[ $(git --git-dir="$stage/remote.git" rev-parse main) == "$(git rev-parse HEAD)" ]]
[[ ! -d .git/frona-crates-release ]]
git diff "$initial" HEAD -- Cargo.toml | diff - /dev/null
echo 'ok - auto release commits, publishes selected dependents and pushes tags'

fixture
export CHANGE=patch
run minor
grep -Fxq frona-text@0.2.0 "$RELEASE_TEST_STATE/uploads"
[[ $(git tag | wc -l) == 3 ]]
echo 'ok - explicit bump starts from committed versions, not automatic proposals'

for failure in REGISTRY_ERROR TEST_FAILURE VERIFY_FAILURE; do
  fixture new
  export "$failure=true"
  if run; then cat "$stage/output"; exit 1; fi
  unchanged
  [[ ! -d .git/frona-crates-release ]]
done
echo 'ok - registry, test and verification failures leave checkout untouched'

fixture new
export PUBLISH_FAILURE=true
if run; then exit 1; fi
release_commit=$(git rev-parse HEAD)
[[ "$release_commit" != "$initial" && -f .git/frona-crates-release/plan.json ]]
[[ $(wc -l < "$RELEASE_TEST_STATE/uploads") == 1 ]]
run --dry-run
[[ $(wc -l < "$RELEASE_TEST_STATE/uploads") == 1 ]]
unset PUBLISH_FAILURE
run
[[ $(git rev-parse HEAD) == "$release_commit" ]]
[[ $(wc -l < "$RELEASE_TEST_STATE/uploads") == 5 ]]
[[ $(wc -l < "$RELEASE_TEST_STATE/updates") == 1 ]]
[[ $(git tag | wc -l) == 5 ]]
echo 'ok - partial publication resumes unchanged plan, skipping uploaded versions'

fixture new
printf '#!/usr/bin/env bash\nexit 1\n' > "$stage/remote.git/hooks/pre-receive"
chmod +x "$stage/remote.git/hooks/pre-receive"
if run; then exit 1; fi
release_commit=$(git rev-parse HEAD)
[[ $(wc -l < "$RELEASE_TEST_STATE/uploads") == 5 ]]
rm "$stage/remote.git/hooks/pre-receive"
run
[[ $(git rev-parse HEAD) == "$release_commit" ]]
[[ $(wc -l < "$RELEASE_TEST_STATE/uploads") == 5 ]]
[[ $(git --git-dir="$stage/remote.git" tag | wc -l) == 5 ]]
echo 'ok - rejected atomic push retries without uploading or bumping again'

fixture new
git tag frona-text-v0.1.0
git push -q origin refs/tags/frona-text-v0.1.0
git tag -d frona-text-v0.1.0 >/dev/null
if run; then exit 1; fi
[[ ! -e "$RELEASE_TEST_STATE/uploads" ]]
grep -q 'Conflicting remote tag' "$stage/output"
echo 'ok - conflicting remote release tags stop publication'

fixture new
git checkout -qb feature
if run; then exit 1; fi
run --dry-run
echo edit >> Cargo.toml
if run --dry-run; then exit 1; fi
echo 'ok - publication requires main; all runs require a clean checkout'
echo 'All library release checks passed.'
