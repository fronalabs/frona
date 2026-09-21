#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DOCKERFILE="$SCRIPT_DIR/Dockerfile"
PKGS_DIR="$SCRIPT_DIR/pkgs"
DRY_RUN=false

usage() {
	echo "Usage: $(basename "$0") [--dry-run]"
	echo "  --dry-run  Show resolved versions without writing files"
	exit 0
}

parse_args() {
	for arg in "$@"; do
		case "$arg" in
		--dry-run) DRY_RUN=true ;;
		--help | -h) usage ;;
		*)
			echo "Unknown option: $arg"
			usage
			;;
		esac
	done
}

# Extract package names from a package list file (strips =version and comments)
pkg_names() {
	grep -vE '^\s*(#|$)' "$1" | cut -d= -f1 | tr '\n' ' '
}

# Parse apt-cache policy output for a given package name
apt_version_from_cache() {
	local pkg="$1"
	local escaped
	escaped=$(printf '%s' "$pkg" | sed 's/[+]/\\+/g')
	echo "$APT_CACHE" | awk "/^${escaped}:/{found=1} found && /Candidate:/{print \$2; exit}"
}

update_apt_file() {
	local file="$1"
	local image="$2"
	local pre_cmd="${3:-}"
	local label
	label=$(basename "$file")

	echo "  [$label] Querying from $image..."
	local names changed=false
	names=$(pkg_names "$file")
	local prep="apt-get update -qq >/dev/null 2>&1 && apt-get install -y --no-install-recommends curl ca-certificates gnupg >/dev/null 2>&1"
	APT_CACHE=$(docker run --rm "$image" bash -c \
		"${pre_cmd:+$prep && $pre_cmd >/dev/null 2>&1 && }apt-get update -qq >/dev/null 2>&1 && apt-cache policy $names")

	local tmpfile
	tmpfile=$(mktemp)
	while IFS= read -r line; do
		if [[ "$line" =~ ^[[:space:]]*(#|$) ]]; then
			echo "$line" >>"$tmpfile"
			continue
		fi
		local pkg="${line%%=*}"
		local old_ver="${line#*=}"
		local new_ver
		new_ver=$(apt_version_from_cache "$pkg")
		if [[ -z "$new_ver" ]]; then
			echo "    WARNING: could not resolve $pkg, keeping current" >&2
			echo "$line" >>"$tmpfile"
		else
			if [[ "$old_ver" != "$new_ver" ]]; then
				echo "    $pkg: $old_ver -> $new_ver"
				changed=true
			fi
			echo "${pkg}=${new_ver}" >>"$tmpfile"
		fi
	done <"$file"

	[[ "$changed" == false ]] && echo "    (no changes)"
	write_or_print "$file" "$tmpfile"
}

update_cargo_file() {
	local file="$1"
	local label
	label=$(basename "$file")

	echo "  [$label] Querying crates.io..."
	local tmpfile changed=false
	tmpfile=$(mktemp)
	while IFS= read -r line; do
		if [[ "$line" =~ ^[[:space:]]*(#|$) ]]; then
			echo "$line" >>"$tmpfile"
			continue
		fi
		local crate="${line%%=*}"
		local old_ver="${line#*=}"
		local new_ver
		new_ver=$(curl -sf -H 'User-Agent: frona-update-versions (https://github.com/fronalabs/frona)' "https://crates.io/api/v1/crates/$crate" |
			python3 -c "import sys,json; print(json.load(sys.stdin)['crate']['max_stable_version'])")
		if [[ -z "$new_ver" ]]; then
			echo "    WARNING: could not resolve $crate, keeping current" >&2
			echo "$line" >>"$tmpfile"
		else
			if [[ "$old_ver" != "$new_ver" ]]; then
				echo "    $crate: $old_ver -> $new_ver"
				changed=true
			fi
			echo "${crate}=${new_ver}" >>"$tmpfile"
		fi
	done <"$file"

	[[ "$changed" == false ]] && echo "    (no changes)"
	write_or_print "$file" "$tmpfile"
}

update_pip_file() {
	local file="$1"
	local image="$2"
	local label
	label=$(basename "$file")

	echo "  [$label] Querying from $image..."
	local packages
	packages=$(awk -F'==' '{print $1}' "$file" | tr '\n' ' ')
	local pip_output
	pip_output=$(docker run --rm "$image" bash -c \
		"pip install --dry-run --no-deps $packages 2>/dev/null | grep 'Would install'" || true)

	if [[ -z "$pip_output" ]]; then
		echo "    (no changes)"
		return
	fi

	local tmpfile changed=false
	tmpfile=$(mktemp)
	# Parse "Would install pkg-1.2.3 pkg2-4.5.6" into requirements format
	echo "$pip_output" | sed 's/Would install //' | tr ' ' '\n' |
		sed 's/-\([0-9]\)/==\1/' | sort >"$tmpfile"

	# Show changes
	while IFS= read -r line; do
		local pkg="${line%%==*}"
		local new_ver="${line#*==}"
		local old_ver
		old_ver=$(awk -F'==' "/^${pkg}==/{print \$2}" "$file" || true)
		if [[ "$old_ver" != "$new_ver" ]]; then
			echo "    $pkg: ${old_ver:-new} -> $new_ver"
			changed=true
		fi
	done <"$tmpfile"

	[[ "$changed" == false ]] && echo "    (no changes)"
	write_or_print "$file" "$tmpfile"
}

latest_op_version() {
	curl -sf -H 'User-Agent: frona-update-versions (https://github.com/fronalabs/frona)' \
		"https://releases.1password.com/developers/cli/" |
		python3 -c 'import re, sys; match = re.search(r"Updated to ([0-9]+\.[0-9]+\.[0-9]+) on", sys.stdin.read()); print(match.group(1) if match else ""); raise SystemExit(0 if match else 1)'
}

latest_bw_version() {
	curl -sf -H 'User-Agent: frona-update-versions (https://github.com/fronalabs/frona)' \
		"https://api.github.com/repos/bitwarden/clients/releases?per_page=100" |
		python3 -c 'import json, sys; releases = json.load(sys.stdin); print(next(item["tag_name"].removeprefix("cli-v") for item in releases if item["tag_name"].startswith("cli-v")))'
}

latest_syd_binary_version() {
	local versions version
	versions=$(curl -sf -H 'User-Agent: frona-update-versions (https://github.com/fronalabs/frona)' \
		"https://gitlab.exherbo.org/api/v4/projects/sydbox%2Fsydbox/repository/tags?per_page=100" |
		python3 -c 'import json, re, sys; versions = [item["name"][1:] for item in json.load(sys.stdin) if re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", item["name"])]; print("\n".join(sorted(versions, key=lambda value: tuple(map(int, value.split("."))), reverse=True)))')

	while IFS= read -r version; do
		if curl -sfIL "https://distfiles.exherbo.org/sydbox%2Fsyd-${version}-x86_64-pc-linux-gnu.tar.xz" >/dev/null &&
			curl -sfIL "https://distfiles.exherbo.org/sydbox%2Fsyd-${version}-aarch64-unknown-linux-gnueabi.tar.xz" >/dev/null; then
			echo "$version"
			return 0
		fi
	done <<<"$versions"

	return 1
}

resolve_binary_version() {
	local package="$1"
	local version

	case "$package" in
	op)
		version=$(latest_op_version)
		curl -sfIL "https://cache.agilebits.com/dist/1P/op2/pkg/v${version}/op_linux_amd64_v${version}.zip" >/dev/null
		curl -sfIL "https://cache.agilebits.com/dist/1P/op2/pkg/v${version}/op_linux_arm64_v${version}.zip" >/dev/null
		;;
	bw)
		version=$(latest_bw_version)
		curl -sfIL "https://github.com/bitwarden/clients/releases/download/cli-v${version}/bw-linux-${version}.zip" >/dev/null
		curl -sfIL "https://github.com/bitwarden/clients/releases/download/cli-v${version}/bw-linux-arm64-${version}.zip" >/dev/null
		;;
	syd) version=$(latest_syd_binary_version) ;;
	*) return 1 ;;
	esac

	echo "$version"
}

update_binary_file() {
	local file="$1"
	local label
	label=$(basename "$file")

	echo "  [$label] Querying release sources..."
	local tmpfile changed=false
	tmpfile=$(mktemp)
	while IFS= read -r line; do
		if [[ "$line" =~ ^[[:space:]]*(#|$) ]]; then
			echo "$line" >>"$tmpfile"
			continue
		fi
		local package="${line%%=*}"
		local old_ver="${line#*=}"
		local new_ver
		if ! new_ver=$(resolve_binary_version "$package"); then
			echo "    WARNING: could not resolve $package, keeping current" >&2
			echo "$line" >>"$tmpfile"
			continue
		fi
		if [[ "$old_ver" != "$new_ver" ]]; then
			echo "    $package: $old_ver -> $new_ver"
			changed=true
		fi
		echo "${package}=${new_ver}" >>"$tmpfile"
	done <"$file"

	[[ "$changed" == false ]] && echo "    (no changes)"
	write_or_print "$file" "$tmpfile"
}

write_or_print() {
	local target="$1"
	local tmpfile="$2"

	if [[ "$DRY_RUN" == true ]]; then
		rm -f "$tmpfile"
	else
		mv "$tmpfile" "$target"
	fi
}

main() {
	parse_args "$@"

	local rust_image python_image node_major
	rust_image=$(awk '/^FROM .* AS planner/{print $2}' "$DOCKERFILE")
	python_image=$(awk '/^FROM .* AS python-builder/{print $2}' "$DOCKERFILE")
	node_major=$(grep 'nodesource.com/setup_' "$DOCKERFILE" | head -1 | sed 's/.*setup_\([0-9]*\).*/\1/')

	echo "==> Base images (from Dockerfile):"
	echo "    rust:   $rust_image"
	echo "    python: $python_image"
	echo "    node:   ${node_major}.x (via NodeSource)"
	echo ""

	local nodesource_setup="curl -fsSL https://deb.nodesource.com/setup_${node_major}.x 2>/dev/null | bash -"

	[[ "$DRY_RUN" == true ]] && echo "==> Dry run — files will NOT be written"
	echo "==> Querying latest versions..."

	update_binary_file "$PKGS_DIR/prod-pkgs.txt"
	update_apt_file "$PKGS_DIR/builder-rust-apt.txt" "$rust_image"
	update_apt_file "$PKGS_DIR/builder-python-apt.txt" "$python_image"
	update_apt_file "$PKGS_DIR/prod-apt.txt" "$python_image" "$nodesource_setup"
	update_apt_file "$SCRIPT_DIR/dev/pkgs/apt.txt" "$python_image"
	update_cargo_file "$PKGS_DIR/builder-rust-cargo.txt"
	update_cargo_file "$SCRIPT_DIR/dev/pkgs/rust-cargo.txt"
	update_pip_file "$PKGS_DIR/builder-python-pip.txt" "$python_image"

	echo ""
	echo "==> Done."
}

main "$@"
