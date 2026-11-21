#!/bin/sh
# Installs model-router and its `mr` alias on Linux or macOS from the release
# download bucket (Cloudflare R2). The repository is private, so GitHub release
# assets need a token; .github/workflows/r2.yml mirrors each release to the
# bucket and uploads this script next to it.
#
#   curl --proto '=https' --tlsv1.2 -LsSf https://DOWNLOADS/install.sh | sh
#   curl ... | sh -s -- --version 0.2.0 --to ~/.local/bin
#
# Environment (flags win):
#   MODEL_ROUTER_VERSION       version to install (default: latest)
#   MODEL_ROUTER_INSTALL_DIR   install directory (default: $CARGO_HOME/bin, else ~/.cargo/bin)
#   MODEL_ROUTER_DOWNLOAD_URL  bucket URL (default: filled in when the script is uploaded)
set -eu

# r2.yml replaces this line with the bucket's public URL when it uploads the script.
DEFAULT_DOWNLOAD_URL=""

say() { printf 'model-router: %s\n' "$*" >&2; }
die() {
	say "error: $*"
	exit 1
}

usage() {
	cat <<'EOF'
Install model-router and the mr alias.

Usage: install.sh [--version X.Y.Z] [--to DIR]

  --version X.Y.Z   install this version instead of the latest
  --to DIR          install into DIR (default: $CARGO_HOME/bin or ~/.cargo/bin)
  -h, --help        show this help
EOF
}

base=${MODEL_ROUTER_DOWNLOAD_URL:-$DEFAULT_DOWNLOAD_URL}
version=${MODEL_ROUTER_VERSION:-}
dir=${MODEL_ROUTER_INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}

while [ $# -gt 0 ]; do
	case $1 in
	--version)
		[ $# -ge 2 ] || die "--version needs a value"
		version=$2
		shift 2
		;;
	--version=*)
		version=${1#*=}
		shift
		;;
	--to)
		[ $# -ge 2 ] || die "--to needs a directory"
		dir=$2
		shift 2
		;;
	--to=*)
		dir=${1#*=}
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*) die "unknown option: $1 (see --help)" ;;
	esac
done

[ -n "$base" ] || die "no download URL; set MODEL_ROUTER_DOWNLOAD_URL to the release bucket's URL"
base=${base%/}
# HTTPS only; plain HTTP is allowed for a loopback bucket used in testing.
case $base in
https://*) proto='=https' ;;
http://127.0.0.1:* | http://127.0.0.1/* | http://localhost:* | http://localhost/*) proto='=http' ;;
*) die "download URL must use https: $base" ;;
esac

case $(uname -s) in
Linux) os=unknown-linux-musl ;;
Darwin) os=apple-darwin ;;
*) die "unsupported OS: $(uname -s). model-router runs on Linux and macOS; on Windows, install it inside WSL" ;;
esac
case $(uname -m) in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) die "unsupported CPU: $(uname -m). Builds exist for x86_64 and aarch64" ;;
esac
# An x86_64 shell under Rosetta on Apple silicon still gets the native build.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
	[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
	arch=aarch64
fi
target=$arch-$os

if command -v curl >/dev/null 2>&1; then
	fetch() { curl --proto "$proto" --tlsv1.2 -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
	if [ "$proto" = '=https' ]; then
		fetch() { wget --https-only -q "$1" -O "$2"; }
	else
		fetch() { wget -q "$1" -O "$2"; }
	fi
else
	die "needs curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
	die "needs sha256sum or shasum to verify the download"
fi
# GNU tar on Linux hands .tar.xz to xz; macOS tar reads it natively.
if [ "$os" = unknown-linux-musl ] && ! command -v xz >/dev/null 2>&1; then
	die "needs xz to unpack the release (install xz-utils or xz)"
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t model-router)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

if [ -z "$version" ]; then
	fetch "$base/latest.txt" "$tmp/latest.txt" || die "could not read $base/latest.txt"
	version=$(tr -d ' \t\r\n' <"$tmp/latest.txt")
	[ -n "$version" ] || die "$base/latest.txt is empty"
fi
tag=v${version#v}

archive=model-router-$target.tar.xz
say "downloading $tag for $target"
fetch "$base/$tag/$archive" "$tmp/$archive" || die "could not download $base/$tag/$archive"
fetch "$base/$tag/$archive.sha256" "$tmp/$archive.sha256" || die "could not download the checksum for $archive"

expected=$(cut -d ' ' -f 1 <"$tmp/$archive.sha256")
actual=$(sha256 "$tmp/$archive")
[ -n "$expected" ] && [ "$expected" = "$actual" ] ||
	die "checksum mismatch for $archive (expected $expected, got $actual)"

mkdir "$tmp/unpack"
tar -xJf "$tmp/$archive" -C "$tmp/unpack"
bin=$(find "$tmp/unpack" -type f -name model-router | head -n 1)
[ -n "$bin" ] || die "$archive has no model-router binary"

mkdir -p "$dir"
# Copy next to the destination, then rename, so a running mr keeps its binary.
cp "$bin" "$dir/.model-router.$$"
chmod 755 "$dir/.model-router.$$"
mv -f "$dir/.model-router.$$" "$dir/model-router"

if [ -e "$dir/mr" ] && [ "$(readlink "$dir/mr" 2>/dev/null || true)" != model-router ]; then
	say "left $dir/mr alone: it is not a link to model-router"
else
	ln -sf model-router "$dir/mr"
fi

say "installed $("$dir/model-router" --version 2>/dev/null || echo "model-router $tag") to $dir"
case ":$PATH:" in
*":$dir:"*) ;;
*) say "$dir is not on PATH; add it to your shell profile, then run mr in a repository" ;;
esac
