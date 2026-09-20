#!/usr/bin/env bash
set -euo pipefail

# Native release artifact check, not a production image build or deployment.
# Host libraries match the native build; the child has no external networking.
artifact_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
artifact_runtime="${CONTAINER_RUNTIME:-podman}"
case "$(uname -m)" in
  aarch64) artifact_libdir=/lib/aarch64-linux-gnu; artifact_loader=ld-linux-aarch64.so.1 ;;
  x86_64) artifact_libdir=/lib/x86_64-linux-gnu; artifact_loader=ld-linux-x86-64.so.2 ;;
  *) echo 'This native artifact check supports Linux aarch64/x86_64.' >&2; exit 1 ;;
esac
test -x "$artifact_root/target/release/frona"
test -f "$artifact_root/web/target/out/index.html"
artifact_catalogs="$artifact_root/target/artifact-catalogs"
cargo run --manifest-path "$artifact_root/Cargo.toml" -p frona-model-catalog -- download --output "$artifact_catalogs"
exec "$artifact_runtime" run --rm --network none \
  -v "$artifact_root/target/release/frona:/artifact/frona:ro" \
  -v "$artifact_root/web/target/out:/artifact/static:ro" \
  -v "$artifact_catalogs:/artifact/catalogs:ro" \
  -v "$artifact_root/build/validate-provider-artifact.mjs:/artifact/validate.mjs:ro" \
  -v "$artifact_libdir:/host-libs:ro" \
  -v /etc/ssl/certs:/etc/ssl/certs:ro \
  -e "FRONA_ARTIFACT_LOADER=/host-libs/$artifact_loader" \
  docker.io/library/node:24.19.0-slim node /artifact/validate.mjs /artifact/frona
