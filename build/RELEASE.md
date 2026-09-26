# Release Process

## Versioning Scheme

Frona uses **CalVer** in the format `YYYY.M.PATCH`:

- `YYYY` — full year of the release
- `M` — month of the release (no zero-padding: `5`, not `05`)
- `PATCH` — sequential in-month patch counter, resets to `0` on the first release of a new month

So `2026.5.0` is the first May 2026 release, `2026.5.1` is the first follow-up patch in May, and `2026.6.0` opens June 2026.

Frona ships as an application (Docker image to GHCR), not a library — there is no API-compatibility contract for SemVer to encode, so the date of the release is the more useful signal.

## Quick Start

```bash
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

```bash
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

```bash
export FRONA_IMAGE=registry.example.com/team/frona
export FRONA_SOURCE_URL=https://forge.example.com/team/frona
mise run release
```

`FRONA_IMAGE` is an image repository without a tag. Git publication uses `origin`.
Staged preparation records that remote's push URL, the image repository, and the
source URL in the artifact. Build and push workers use those recorded destinations,
regardless of their own `origin` or environment overrides.

## Version Sources

The script updates these files in sync:

- `Cargo.toml` — `version` under `[workspace.package]`
- `web/package.json` — `"version"` field
- `web/package-lock.json` — root `"version"` + `packages[""]` version

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

```bash
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

```bash
# AMD64 worker
mise run release:build --release /tmp/frona-release --arch amd64 \
  --output /tmp/amd64.json

# ARM64 worker
mise run release:build --release /tmp/frona-release --arch arm64 \
  --output /tmp/arm64.json
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

```bash
mise run release:push --release /tmp/frona-release \
  --results /tmp/amd64.json /tmp/arm64.json
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

## Release script tests

```bash
mise run test:release
```

`build/test-release.sh` uses Bash and jq with actual temporary Git repositories
and bare remotes with fake
Cargo and Docker executables. They cover existing release commands and rollback,
staged artifacts, explicit fork destinations, build/publish failures, validation,
atomic Git push rejection, and retries. They do not build real images or publish a release.
