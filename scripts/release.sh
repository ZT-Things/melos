#!/usr/bin/env bash
# Build release artifacts for a melos version.
# Usage: ./scripts/release.sh 0.1.0 [--no-build]
set -euo pipefail

version="${1:-}"
if [[ -z "$version" ]]; then
    echo "usage: $0 <version> [--no-build]" >&2
    exit 2
fi
if [[ -n "$(git status --porcelain)" ]]; then
    echo "error: working tree is dirty; commit first" >&2
    exit 1
fi
if ! git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
    echo "error: tag v$version does not exist" >&2
    exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
dist="$root/dist"
rm -rf "$dist"
mkdir -p "$dist"

echo "==> source tarball"
git archive --format=tar.gz --prefix="melos-$version/" \
    -o "$dist/melos-$version.tar.gz" "v$version"

echo "==> release build"
if [[ "${2:-}" != "--no-build" ]]; then
    cargo build --release --locked --workspace
fi
if [[ ! -x target/release/melos || ! -x target/release/melosd ]]; then
    echo "error: target/release binaries missing" >&2
    exit 1
fi

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/melos-$version"
install -m755 target/release/melos "$stage/melos-$version/"
install -m755 target/release/melosd "$stage/melos-$version/"
install -m644 README.md LICENSE config.toml.example "$stage/melos-$version/"
cp -a packaging "$stage/melos-$version/"
rm -rf "$stage/melos-$version/packaging/melos-bin" \
       "$stage/melos-$version/packaging/melos-git"
tar -C "$stage" -czf \
    "$dist/melos-$version-x86_64-unknown-linux-gnu.tar.gz" "melos-$version"

echo "==> checksums"
(cd "$dist" && sha256sum ./*.tar.gz | tee sha256sums.txt)
echo
echo "artifacts in $dist"
echo "next: create the GitHub release, upload the tarballs, then run updpkgsums"
