# Build

## How the Build Works

The Dockerfile is a multi-stage build with two final targets: `dev` and `prod`.

1. **frontend-builder** — `npm ci` + `npm run build` to produce a static export
2. **planner** — `cargo chef prepare` to fingerprint Rust dependencies
3. **backend-builder** — `cargo chef cook` (cached dependency build) then `cargo build --release`
4. **cli-tools** — downloads arch-specific binaries (1Password CLI, Bitwarden CLI, SydBox, SurrealDB) using Docker's `TARGETARCH`
5. **python-builder** — pip installs into a `/install` prefix
6. **prod** — minimal `python:3.12-slim-bookworm` image with the compiled binary, static frontend, CLI tools, and Python packages
7. **dev** — full `rust:1.89-bookworm` toolchain with cargo-watch hot-reload and Node.js

Rust dependency caching relies on [cargo-chef](https://github.com/LukeMathWalker/cargo-chef) — dependencies are compiled once from `recipe.json` and cached across builds as long as `Cargo.toml`/`Cargo.lock` don't change.

## Version Pinning

Build dependencies are version-locked in `build/pkgs/` text files (`name=version` format):

- `prod-pkgs.txt` — CLI tool versions (op, bw, syd)
- `builder-rust-cargo.txt` — Cargo tools (cargo-chef)
- `builder-python-pip.txt` — Python packages (pandas, numpy, scipy, etc.)

SurrealDB is the exception — its version is extracted from `Cargo.lock` at build time so it always matches the Rust dependency.

Base images are pinned to major/minor versions. APT packages (`*-apt.txt`) are not version-pinned and resolve at build time.

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

## Development files

Development-only files live in `build/dev/`:

- `watch.sh`: Rust rebuilds.
- `docker-compose.podman.yml`: rootless Podman development overrides.
- `install-kache.sh` and `kache.toml`: compiler-cache setup.
- `searxng-settings.yml`: search-service settings.
- `pkgs/apt.txt` and `pkgs/rust-cargo.txt`: development package lists.
- `test-container.py`: launcher and provider regression checks.

The shared Dockerfile, Compose definition, and container launcher stay in
`build/`. Builder and production package lists stay in `build/pkgs/`;
`update-versions.sh` updates both package directories.
