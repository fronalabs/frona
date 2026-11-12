# Release Process

## Two versioning schemes

The application and published Rust libraries have independent release versions.

| Release | Version scheme | Version source | Preparation command |
|---------|----------------|----------------|---------------------|
| Frona application and container images | CalVer: `YYYY.M.PATCH` | Root `Cargo.toml` workspace version and frontend package files | `mise run release` or `mise run release:prepare` |
| `frona-text`, `frona-derive`, `frona-model-catalog`, `frona-harness`, `frona-pkm` | SemVer: `MAJOR.MINOR.PATCH`, initially `0.1.0` | Each crate's own `Cargo.toml` | `mise run release:crates` |

Application releases do not bump library versions. Library version preparation
does not bump the application version or build container images. Libraries can
have different versions and release independently. `frona-ontologies` is published
from its own repository and consumed as a registry dependency.

## Application versioning

The application uses **CalVer** in the format `YYYY.M.PATCH`:

- `YYYY` — full year of the release
- `M` — month of the release (no zero-padding: `5`, not `05`)
- `PATCH` — sequential in-month patch counter, resets to `0` on the first release of a new month

So `2026.5.0` is the first May 2026 release, `2026.5.1` is the first follow-up patch in May, and `2026.6.0` opens June 2026.

The application ships as a Docker image to GHCR. Its version identifies an
application release; the published libraries use SemVer to communicate API compatibility.

## Application quick start

```nu
mise run release                          # cut today's release; auto-rolls month or bumps PATCH
mise run release patch                    # 2026.5.0 → 2026.5.1 (strict: errors on month change)
mise run release alpha                    # start or advance an alpha pre-release
mise run release beta                     # advance to beta
mise run release rc                       # advance to rc
mise run release stable                   # 2026.5.0-RC1 → 2026.5.0
mise run release 2026.5.0-RC1             # explicit version
```

## CLI Usage

```
build/release.sh [command] [--dry-run] [--skip-docker] [--skip-tests]
```

### Commands

| Command | Description |
|---------|-------------|
| *(no arg)* / `today` | Cut today's release; bumps `PATCH` within the current `YYYY.M`, or resets to `YYYY.M.0` when the month rolls over |
| `patch` | Increment `PATCH` within the current `YYYY.M`. Strict: errors if the calendar month has rolled over since the last release — use the no-arg form instead |
| `alpha` / `beta` / `rc` | Start or advance a pre-release. Rolls to today's `YYYY.M` if the current version is already stable |
| `stable` | Promote current pre-release to stable |
| `<version>` | Set an explicit version (e.g., `2026.5.0-RC1`) |

### Flags

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview changes without modifying anything |
| `--skip-docker` | Version bump + git tag only, no Docker build |
| `--skip-tests` | Skip `cargo test` before releasing |

## Pre-release Format

Pre-release versions use FreeBSD-style uppercase tags without dot separators:

- `2026.5.0-ALPHA1`, `2026.5.0-BETA2`, `2026.5.0-RC1`

Commands are lowercase for ergonomics (`mise run release alpha`).

## Pre-release Workflow

```nu
# Start a pre-release series (rolls to today's YYYY.M, bumps PATCH if same month)
mise run release alpha                    # 2026.5.0 → 2026.5.1-ALPHA1

# Iterate within a pre-release tag
mise run release alpha                    # 2026.5.1-ALPHA1 → 2026.5.1-ALPHA2

# Advance to the next stage
mise run release beta                     # 2026.5.1-ALPHA2 → 2026.5.1-BETA1
mise run release rc                       # 2026.5.1-BETA1 → 2026.5.1-RC1

# Promote to stable
mise run release stable                   # 2026.5.1-RC1 → 2026.5.1
```

## Docker Tagging

- **Stable** `2026.5.0` → `<image>:v2026.5.0` + `:latest`
- **Pre-release** `2026.5.0-ALPHA1` → `<image>:v2026.5.0-ALPHA1` only (no `:latest`)

The default image is `ghcr.io/fronalabs/frona`, and its OCI source annotation is
`https://github.com/fronalabs/frona`. Set these explicitly for a fork or another
registry; neither value is inferred from the Git remote:

```nu
$env.FRONA_IMAGE = "registry.example.com/team/frona"
$env.FRONA_SOURCE_URL = "https://forge.example.com/team/frona"
mise run release
```

`FRONA_IMAGE` is an image repository without a tag. Git publication uses `origin`.
Staged preparation records that remote's push URL, the image repository, and the
source URL in the artifact. Build and push workers use those recorded destinations,
regardless of their own `origin` or environment overrides.

## Application version sources

The script updates these files in sync:

- `Cargo.toml` — `version` under `[workspace.package]`
- `web/package.json` — `"version"` field
- `web/package-lock.json` — root `"version"` + `packages[""]` version

It also refreshes the workspace `Cargo.lock`. The five publishable libraries
declare explicit versions rather than inheriting `[workspace.package].version`.

## Safety Checks

1. Working tree must be clean (no uncommitted changes)
2. Stable releases must be from the `main` branch
3. Git tag must not already exist (also blocks accidental same-day re-runs)
4. Tests must pass (unless `--skip-tests`)

## Git Operations

- Commit message: `release: v{version}`
- Annotated tag: `v{version}`
- Auto-pushes commit and tag to `origin`

## Staged releases for CI

`mise run release` still runs the entire release locally: tests, version updates,
local commit/tag, multi-platform image build and publication, then Git push. Its
existing commands and `--dry-run`, `--skip-tests`, and `--skip-docker` flags remain
supported. Image build failure rolls back the local release commit/tag as before.
Git pushes now use `--atomic` so the branch and tag are accepted together.

CI can run the same release implementation across native workers using these
additional tasks. Frona owns versioning, image validation and publication, and Git
operations. The CI workflow owns scheduling, credentials, tool installation,
Docker access, and artifact transport. No Gitea-specific client is required.

### 1. Configure credentials and prepare

Use a full source checkout with tags, on the intended branch (not a detached
commit). Configure `git user.name` and `user.email`. Rust and native test
dependencies must be available, as for the ordinary release command.

The caller configures Git and Docker authentication and runs any permission
checks before preparation. Frona does not read CI token variables or call a
hosting provider's permission API. Local releases use your existing credentials;
each CI system can supply its own credentials through standard Git and Docker
configuration.

Staged operations create temporary Git repositories. Git credentials must also
work there, for example through an SSH agent, a global credential helper, or
process-wide Git configuration. Authentication stored only in the checkout's
`.git/config` does not carry over. Keep credentials out of remote URLs because
preparation records the push URL in the artifact.

```nu
# For a fork, set FRONA_IMAGE and FRONA_SOURCE_URL before this command.
# Output must be a new directory outside the source checkout.
mise run release:prepare today --output /tmp/frona-release
```

Staged release scripts use Bash and jq. They do not require Python or curl.

`release:prepare` runs the shared versioning and test logic in a temporary clone.
It leaves the caller's branch, tags, and worktree unchanged and pushes nothing.
The output contains:

- `release.bundle`: a self-contained Git bundle with the release commit, its
  history, and the exact annotated tag object.
- `release.json`: schema version, base/release commits, tag identity, version,
  Git push URL, image destination/tags, source URL, required platforms, build
  timestamp, and bundle checksum.

The prepare command accepts the same version commands as `release`, plus
`--skip-tests` and `--dry-run`. The other staged commands reject these two flags.
A dry run creates no artifact. Existing output
directories are never overwritten. Retain the prepared artifact for retries;
do not prepare the same release separately on each worker.

### 2. Build on native workers

Transfer the entire prepared directory unchanged to both workers. Each worker
needs Bash, jq, Git, Docker Buildx, a native Docker engine, and registry
login. It does not need a host Rust installation; image compilation remains in
the existing Dockerfile. Authenticate Docker in each job using its temporary
Docker config, then run:

```nu
# AMD64 worker
mise run release:build --release /tmp/frona-release --arch amd64 --output /tmp/amd64.json

# ARM64 worker
mise run release:build --release /tmp/frona-release --arch arm64 --output /tmp/arm64.json
```

Jobs without Mise can call `bash build/release.sh build` with the same arguments.
Build and push stages need a checkout of Frona's release tools; the source they
build/publish is imported from the bundle, not taken from that checkout's HEAD.

Each build verifies the artifact, checks the Docker engine's native architecture,
and uses the shared release Docker invocation. Images are uploaded by digest only;
neither the version image tag nor `latest` moves. A successful result records the
prepared commit/tag, image repository, platform, and immutable image digest.
A failed build writes no result. Use a new output filename when rebuilding.

The stage uses a `docker-container` builder named `frona-release-amd64` or
`frona-release-arm64`, removing its client definition while retaining its cache
volume afterward. These names are separate from the normal `multiarch` builder.
Schedule only one release build at a time on each worker/daemon for these names.
The ordinary release command continues using `multiarch` by default; all shared
build scripts accept `FRONA_BUILDER` when an explicitly configured builder is needed.

### 3. Publish images, then push Git

After both workers succeed, give a final job the original prepared directory and
both result files. Supply Git write credentials for the recorded remote and
authenticate Docker to the recorded image registry. No Rust toolchain is needed.

```nu
mise run release:push --release /tmp/frona-release --results /tmp/amd64.json /tmp/arm64.json
```

Frona verifies the bundle, result identities, both registry images' platforms,
versions and source revisions, and the combined image index before publication.
It also checks that the remote source branch still matches the prepared base and
that the release tag is available. It publishes `v<version>` and, for stable
releases, `latest`, verifies their registry digests, then atomically pushes the
exact prepared commit/tag. It never force-pushes.

Serialize complete release workflows from preparation through push. CI should
use job dependencies so this command is never invoked after a failed build;
Frona also rejects missing, duplicate, or mismatched build results itself.

### Failures and retries

- Failed tests, preparation, or either architecture build leave remote Git refs
  and named release images unchanged. Discarded builds may leave untagged images.
- Retry a failed architecture using the same prepared artifact. Keep the other
  architecture's successful result and use a new output filename for the retry.
- Retry publication with the same artifact and exact build results. A version
  image already published must match the candidate digest; replacing it with a
  different build is rejected, even if the source commit matches.
- A completed release retry is a no-op and never moves `latest` back over a newer
  release. It verifies that the original Git tag and version image still match.
- A moved source branch or conflicting Git tag stops publication. Prepare from
  the updated source after resolving the conflict; no automatic rebase changes
  the tested release commit.
- Registry publication and Git push are not one transaction. If image publication
  succeeds but Git rejects the push, retain the artifacts, resolve permissions,
  and retry `release:push`. If the source branch moved during publication, inspect
  and reconcile that state before making a new release. Images are not deleted
  and Git history is not rewritten automatically.

Artifact checks detect corruption and mixing releases; they do not authenticate
an artifact's producer. Pass artifacts only between trusted jobs in the same run.
Prepared artifacts and build results now use schema 2; regenerate older schema 1
artifacts and their build results before using these tools.

## Library releases

Library versions are independent SemVer versions. The release command detects
changes against crates.io, proposes versions, and includes libraries whose local
dependency requirements need updating. You do not need to select crates manually.

```nu
mise run release:crates -- --dry-run          # preview auto versions, test and verify packages
mise run release:crates                    # auto versions, commit, publish and push tags
mise run release:crates minor              # override the bump for changed published crates
mise run release:crates -- patch --dry-run    # verify an explicit override before releasing
```

`auto` is the default; `patch`, `minor`, and `major` override the bump for selected
published libraries, starting from their committed versions. An unpublished initial
version stays at its declared version (initially `0.1.0`). Unchanged libraries are
skipped. Additional dependency-only releases may receive an automatic patch bump.

### Automatic versions

Mise pins `release-plz`, `cargo-semver-checks`, and `cargo-edit`. Release-plz compares
the packages with their published contents, checks library API compatibility, and
uses commit messages to propose bumps. It also updates dependent requirements,
including the server's, while leaving the application version unchanged.

Use Conventional Commit messages to make intent explicit:

- `fix(text): handle empty input` for fixes.
- `feat(harness): add a new capability` for features.
- `feat(pkm)!: change the phase interface` or a `BREAKING CHANGE:` footer for breaks.

Existing area-prefixed messages such as `pkm: ...` still participate in change
detection, but do not declare feature or breaking-change intent. API checks can
find many breaking changes without that marker; they cannot detect every behavioral
compatibility change. Review the proposal or use an explicit version override.

Before `1.0.0`, the default policy bumps minor for breaking changes and patch for
compatible changes, including features. From `1.0.0`, breaking changes bump major,
features bump minor, and fixes bump patch. An explicit override is your decision;
it can override the automatically recommended bump.

### Preparation and publication

All runs require a clean checkout. Publication requires `main`, Cargo credentials
for crates.io, and permission to push the branch and tags to `origin`. Dry runs can
run on another branch and do not need publication credentials. Install the pinned
tools with `mise install`; the wrapper also uses Bash, Git, jq, curl, and flock.

Preparation happens in a temporary clone containing the current committed state:

1. Discover changes and propose versions, including dependent releases.
2. Refresh the root and standalone harness fixture lockfiles.
3. Run the selected libraries' tests and doctests with default features.
4. Run `cargo publish --dry-run` for the selected libraries together, with build
   verification enabled. Cargo resolves unpublished siblings through its temporary
   registry.

A dry run stops here, leaving the checkout, commits, tags, and registry untouched.
Build outputs and `.crate` archives remain in `target`. Package and test failures
also leave the original checkout untouched.

A real release then creates a `release: libraries` commit, fast-forwards the local
checkout to it, and publishes the selected versions in Cargo's dependency order.
Finally, it creates annotated tags such as `frona-harness-v0.2.0` and atomically
pushes the branch and those tags to `origin`. It does not create Gitea release
entries or application images. The external `frona-ontologies` crate is not released
by this command.

### Retrying a failed release

Once preparation succeeds, the wrapper saves the exact release commit and package
versions under Git's `frona-crates-release` directory. It retains that journal if
publication or the Git push fails. Do not delete it after a partial upload.

Rerun the default command to resume:

```nu
mise run release:crates
```

The retry uses the same commit and versions, checks crates.io, skips versions already
uploaded, and finishes the tags and Git push. It does not propose another bump.
`--dry-run` verifies the remaining packages without advancing the pending release.
Resume on the original branch, at either its original or prepared commit, with a
clean checkout. Version overrides are rejected while a release is pending.

Registry outages are errors, never interpreted as unpublished packages. Published
versions cannot be rolled back, so a partial release is retained for completion
instead of resetting the local release commit.

## Release script tests

```nu
mise run test:release
mise run test:release:crates
```

`build/test-release.sh` covers application releases using temporary repositories,
bare remotes, and fake Cargo/Docker executables.

`build/test-release-crates.sh` covers library selection, overrides, dry runs,
verification failures, partial uploads, push failures, and retries using real Git
repositories with fake Cargo, release-plz, and registry responses. These tests do
not upload packages and do not replace real package verification.
