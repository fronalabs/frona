#!/bin/sh
set -eu
case "$(uname -m)" in
  x86_64) arch=x86_64 ;;
  aarch64) arch=aarch64 ;;
  *) echo "Unsupported Bacon architecture" >&2; exit 1 ;;
esac
stage=$(mktemp -d)
trap 'rm -f "$stage/bacon.zip" "$stage/bacon"; rmdir "$stage"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL --retry 2 \
  "https://github.com/Canop/bacon/releases/download/v3.26.0/bacon_3.26.0.zip" -o "$stage/bacon.zip"
printf '%s  %s\n' a23260e9f23f63b2363830f15c685da4032477d55258d9e646c39f9680f3f75a "$stage/bacon.zip" | sha256sum -c -
unzip -p "$stage/bacon.zip" "${arch}-unknown-linux-musl/bacon" > "$stage/bacon"
install -m 0755 "$stage/bacon" "${1:-/usr/local/bin}/bacon"
"${1:-/usr/local/bin}/bacon" --version
