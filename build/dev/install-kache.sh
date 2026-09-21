#!/bin/sh
set -eu
case "$(uname -m)" in
  x86_64) arch=x86_64; checksum=dcd5e578a74079a288622ac13243f21d4f2861d2cb5fd9f35ba8e6ba71afbd32 ;;
  aarch64) arch=aarch64; checksum=ff11a4ffe22fadf0b8b1982fb767a1c127496ce492d04134b86ec81ab4fbb1ad ;;
  *) echo "Unsupported Kache architecture" >&2; exit 1 ;;
esac
stage=$(mktemp -d)
trap 'rm -f "$stage/kache.tar.gz" "$stage/kache"; rmdir "$stage"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL --retry 2 \
  "https://github.com/kunobi-ninja/kache/releases/download/v0.26.3/kache-${arch}-unknown-linux-musl.tar.gz" -o "$stage/kache.tar.gz"
printf '%s  %s\n' "$checksum" "$stage/kache.tar.gz" | sha256sum -c -
tar -xzf "$stage/kache.tar.gz" -C "$stage" kache
install -m 0755 "$stage/kache" /usr/local/bin/kache
kache --version
