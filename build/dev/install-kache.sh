#!/bin/sh
set -eu
: "${KACHE_VERSION:?Pass the workspace Kache version as the KACHE_VERSION build argument}"
if ! printf '%s\n' "$KACHE_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$'; then
  echo "Invalid Kache version: $KACHE_VERSION" >&2
  exit 1
fi
case "$(uname -m)" in
  x86_64) arch=x86_64 ;;
  aarch64) arch=aarch64 ;;
  *) echo "Unsupported Kache architecture" >&2; exit 1 ;;
esac
stage=$(mktemp -d)
archive="kache-${arch}-unknown-linux-musl.tar.gz"
release="https://github.com/kunobi-ninja/kache/releases/download/v${KACHE_VERSION}"
trap 'rm -f "$stage/$archive" "$stage/$archive.sha256" "$stage/kache"; rmdir "$stage"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL --retry 2 \
  "$release/$archive" -o "$stage/$archive"
curl --proto '=https' --tlsv1.2 -fsSL --retry 2 \
  "$release/$archive.sha256" -o "$stage/$archive.sha256"
# Verify the downloaded archive, regardless of the path in the checksum file.
checksum=$(awk 'NR == 1 {print $1}' "$stage/$archive.sha256")
printf '%s  %s\n' "$checksum" "$stage/$archive" | sha256sum -c -
tar -xzf "$stage/$archive" -C "$stage" kache
install -m 0755 "$stage/kache" "${1:-/usr/local/bin}/kache"
"${1:-/usr/local/bin}/kache" --version
