# Build

## How the Build Works

The Dockerfile is a multi-stage build with two final targets: `dev` and `prod`.

1. **frontend-builder** — `npm ci` + `npm run build` to produce a static export
2. **planner** — `cargo chef prepare` to fingerprint Rust dependencies
3. **backend-builder** — `cargo chef cook` (cached dependency build) then `cargo build --release`
4. **cli-tools** — downloads arch-specific binaries (1Password CLI, Bitwarden CLI, SurrealDB) using Docker's `TARGETARCH`
5. **syd-builder** — compiles the tagged Syd source with upstream's builder image and Makefile for the target architecture
6. **python-builder** — pip installs into a `/install` prefix
7. **prod** — minimal `python:3.12-slim-bookworm` image with the compiled binary, static frontend, CLI tools, and Python packages
8. **dev** — the same Python base with Rust 1.98.1, Bacon hot-reload, and Node.js

Rust dependency caching relies on [cargo-chef](https://github.com/LukeMathWalker/cargo-chef) — dependencies are compiled once from `recipe.json` and cached across builds as long as `Cargo.toml`/`Cargo.lock` don't change.

## Version Pinning

Build dependencies are version-locked in `build/pkgs/` text files (`name=version` format):

- `prod-pkgs.txt` — CLI tool versions (op, bw, syd)
- `builder-rust-cargo.txt` — Cargo tools (cargo-chef)
- `builder-python-pip.txt` — Python packages (pandas, numpy, scipy, etc.)

SurrealDB is the exception — its version is extracted from `Cargo.lock` at build time so it always matches the Rust dependency.

Base image versions are specified in the Dockerfile. APT packages (`*-apt.txt`)
are version-pinned and refreshed by `update-versions.sh`.

Syd's version comes from `prod-pkgs.txt`. The updater selects stable releases
published to crates.io and verifies the matching Git tag archive is available.
The `syd-builder` stage uses upstream's `exherbo/syd-builder` images, pinned by
digest for amd64 and arm64, and the tagged source's `make release` target. It
builds only `syd` with the `trusted` feature and upstream's static linking
defaults. The builder images supply the compiler and libseccomp; Frona does not
maintain a separate Syd build script or dependency toolchain.

The source comes from [Syd's upstream repository](https://gitlab.exherbo.org/sydbox/sydbox).
Its Makefile uses Cargo.lock. Frona runs
`make release CARGOFEATS=trusted CARGOFLAGS="--bin syd"`, strips debug information,
and installs `target/<rust-host-tuple>/release/syd`.

Both images include the Syd binary at `/usr/local/bin/syd` and license notices in
`/usr/local/share/doc/syd/`. Source, dependencies, and build tools remain
in the `syd-builder` stage; no source archive is included in either runtime image.
Syd is a separate GPL-3.0-only executable; Frona retains its BSL license.
Syd's license is in `COPYING`, and the statically linked
[libseccomp](https://github.com/seccomp/libseccomp) LGPL-2.1 license is in
`libseccomp-LICENSE` in that directory.

## Multi-Architecture Docker Builds

Builds use Docker Buildx to produce `linux/amd64` and `linux/arm64` images. By default, a single local builder handles both platforms via QEMU emulation.

### Remote amd64 Builder

For faster amd64 builds, add a remote amd64 server as a native buildx node instead of relying on QEMU.

**Prerequisites:** Docker installed on the remote server, accessible via SSH.

**Setup:**

```bash
# Remove existing builder (if it claims both platforms on one node)
docker buildx rm multiarch

# Local node — arm64 only
docker buildx create --name multiarch --platform linux/arm64

# Remote node — amd64 only
docker buildx create --name multiarch --append \
  --platform linux/amd64 \
  ssh://user@your-amd64-server

docker buildx use multiarch
docker buildx inspect multiarch --bootstrap
```

Each node must be constrained to its native platform with `--platform`. Without this, the local node claims both `linux/arm64` and `linux/amd64`, and buildx routes amd64 builds through QEMU instead of the remote node.

**Verify:** `docker buildx inspect multiarch` should show two nodes, each with a single platform.

### Publishing

```bash
mise run docker:publish
```

This builds for both platforms and pushes to `ghcr.io/fronalabs/frona:latest`. See `publish.sh` for details.

## Releasing

See [RELEASE.md](RELEASE.md) for the full release process, versioning scheme, and Docker tagging strategy.

## Build storage

Frona uses ordinary `npm ci`, `npm install`, and `npm run` commands. In dv,
`target`, `web/node_modules`, `web/.next`, and `web/target` are separate mounts
from one build dataset. Retained `data` uses its own dataset and is never cleaned.

Next development and production intermediates use `web/.next`; production static
exports use `web/target/out`. TypeScript incremental state and Vitest coverage use
`web/target/tsconfig.tsbuildinfo` and `web/target/coverage`. These output settings
also apply to ordinary checkouts and production images. Docker's final image
copies the static export into `/app/static`. Development Compose explicitly
binds all frontend artifact paths beneath the source mount.

`mise run clean` empties the four build directories while preserving mount roots.
`dv clean WORKSPACE` uses the configured build paths and leaves the workspace
running. Stop build processes yourself before cleaning if needed.

## Podman image build parallelism

`mise run container:dev` reuses an existing development image. If it is missing,
the launcher builds it directly with Podman before starting Compose. Explicit
rebuilds (`mise run container:dev:build`) use the same path. Both pass
`--jobs=$(nproc)` so independent Dockerfile stages can run concurrently; set
`CONTAINER_BUILD_JOBS` to override the count (`0` means unlimited stages).
Compose is then started with `--no-build` to avoid a second, serial build.
Passing `--no-build` yourself skips the automatic build entirely.

This controls image stages, not Cargo jobs. Instructions within a stage and
stages that depend on earlier outputs still execute in dependency order.

## Development compiler cache

The development image installs checksum-pinned Kache via `dev/install-kache.sh`.
Compose selects it with `RUSTC_WRAPPER=kache`; `dev/watch.sh` starts its daemon
before compiling. Production builds continue to use cargo-chef.

Each development container has a local cache at `/cache/kache`. `KACHE_LOCAL_DIR`
can select a host directory; dv defaults to `/dv/private/kache/frona-podman`,
while standalone Compose uses the `kache-local` named volume. The filesystem
backend is mounted at `/dv/shared/kache`: `KACHE_SHARED_DIR` selects its source,
defaulting to dv's owner-private shared cache or `target/kache-shared` outside dv.
Other workspaces can reuse artifacts through that shared backend.

`dev/kache.toml` defines cache sizes and daemon behavior. Build-script caching
is disabled and restore verification is enabled. These are intentional
correctness settings, not benchmark switches.

Cargo job and test-thread counts inherit `CARGO_BUILD_JOBS` and `RUST_TEST_THREADS`
without project-specific limits. Development/test profiles retain reduced debug
information. Run the launcher regression checks with Python 3 and Bash. The
provider-parsing check also runs when Podman and podman-compose are installed:

```bash
python3 build/dev/test-container.py
```

## Bacon development workflow

The development image downloads Bacon 3.26.0 from the upstream release archive
and verifies its SHA-256 checksum. Bacon is not built from source or required
on the host. The version and checksum are pinned in `dev/install-bacon.sh`.

From the repository root:

```nu
mise run container:dev
```

Bacon also provides workspace check, test, and Clippy jobs in the development
container. Each Bacon session runs one job.

The shared `bacon.toml` watches crate sources/resources and root Cargo manifests
and lockfile. The container server job waits three seconds before each build to group
nearby writes (Bacon's grace period, not a sliding debounce). Changes stop the
old server, rebuild incrementally through Cargo, and launch a new process.
A failed build leaves the server stopped until a successful rebuild.

Containers use the headless `server-container` job, preserving the Kache setup
and `mcpctl` build/copy steps in `dev/watch.sh`. Rebuild an existing dev image
with `mise run container:dev:build` to install Bacon.
Compose mounts the shared `bacon.toml` read-only at `/app/bacon.toml`.
Startup checks for the binary and job configuration before launching either
watcher, so an old image reports the rebuild command immediately.

Run the isolated watcher regression checks in the built development image,
from the repository root (rootless Podman):

```nu
podman run --rm --network none --userns keep-id --volume .:/app:ro --entrypoint python3 localhost/frona-dev:local build/dev/test-bacon.py
```

These checks exercise restarts, build-failure recovery, file coverage, and child
shutdown using fixture commands; they do not build or start Frona.

## Development shutdown

The development entrypoint starts the Rust and frontend watchers in separate
process groups. SIGINT/SIGTERM stop both groups; if either watcher exits, its
sibling is stopped too.

The container job launches `dev/watch.sh`, which supervises its build and server
processes using `dev/run-command.sh`. The container entrypoint also wraps Bacon
because headless Bacon can exit on a signal without stopping its job. Both wait
for the whole process group. The server job allows ten seconds for graceful
shutdown; the watcher wrapper allows twelve seconds so the job can finish
cleanup first.

For foreground Podman development, exiting `mise run container:dev` also runs
Compose `down` without `--volumes`. This removes stopped containers and the
project network so the next launch does not collide with stale names. Named
volumes, bind-mounted data, and caches are retained. Detached runs (`-d`) and
`--no-start` are not automatically torn down.

## Development files

Development-only files live in `build/dev/`:

- `start.sh`, `run-command.sh`, and `watch.sh`: process supervision and Rust rebuilds.
- `install-bacon.sh`: checksum-pinned prebuilt Bacon installer.
- `docker-compose.podman.yml`: rootless Podman development overrides.
- `install-kache.sh` and `kache.toml`: compiler-cache setup.
- `searxng-settings.yml`: search-service settings.
- `pkgs/apt.txt`: development system packages.
- `test-container.py`: launcher, provider, and shutdown regression checks.
- `test-bacon.py`: Bacon restart, recovery, and shutdown regression checks.

The shared Dockerfile, Compose definition, and container launcher stay in
`build/`. Builder and production package lists stay in `build/pkgs/`;
`update-versions.sh` updates both package directories.

Run `mise run container:update-versions` to refresh the dependency pins, or
`bash build/update-versions.sh --dry-run` to preview changes. The updater uses
Podman when installed, falling back to Docker. Set `CONTAINER_RUNTIME` to
`docker` or `podman` to choose explicitly.
