#!/usr/bin/env bash
# Build a release binary and install it to ~/.cargo/bin (plus `mr` symlink).
#
# Usage: scripts/build.sh [options]
#   -c, --check         run fmt, clippy, and tests first (mirrors CI)
#   -b, --bump LEVEL    bump Cargo.toml version: patch | minor | major
#   -n, --no-install    build target/release/model-router only
#   -h, --help          show this help
set -euo pipefail

cd "$(dirname "$0")/.."

check=0
bump=""
install=1

usage() { sed -n '2,9s/^# \{0,1\}//p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    -c|--check) check=1 ;;
    -b|--bump) bump="${2:-}"; shift ;;
    -n|--no-install) install=0 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

current_version() {
  sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1
}

if [[ -n "$bump" ]]; then
  IFS=. read -r major minor patch <<<"$(current_version)"
  case "$bump" in
    patch) patch=$((patch + 1)) ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    major) major=$((major + 1)); minor=0; patch=0 ;;
    *) echo "invalid bump level: $bump (patch|minor|major)" >&2; exit 2 ;;
  esac
  next="$major.$minor.$patch"
  # Only the [package] version is the first top-level `version =` line.
  sed -i.bak "0,/^version = \".*\"/s//version = \"$next\"/" Cargo.toml
  rm -f Cargo.toml.bak
  # Refresh Cargo.lock so --locked builds accept the new version.
  cargo update --workspace --quiet
  echo "==> version $next"
fi

if [[ $check -eq 1 ]]; then
  echo "==> fmt";    cargo fmt --check
  echo "==> clippy"; cargo clippy --all-targets --locked -- -D warnings
  echo "==> test";   cargo test --locked
fi

echo "==> build release"
cargo build --release --locked

if [[ $install -eq 1 ]]; then
  echo "==> install"
  cargo install --path . --locked --force
  bin_dir="${CARGO_HOME:-$HOME/.cargo}/bin"
  ln -sfn model-router "$bin_dir/mr"
  echo "==> installed $bin_dir/model-router (alias: mr)"
fi

echo "==> model-router $(current_version) ($(git rev-parse --short HEAD 2>/dev/null || echo unknown))"
