#!/usr/bin/env bash
# Builds the release tarball from the working tree: the static binary (scripts/cargo build),
# install.sh, SHA256SUMS of both, README.md, CHANGELOG.md and LICENSE, in one folder
# claude-usage-<version>-x86_64-linux/. Reproducible: sorted names, files dated by the last
# commit, owner 0, fixed modes, gzip without name or time.
#   scripts/tarball.sh [OUT_DIR]     default: ./target/dist
set -euo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
OUT="${1:-$REPO/target/dist}"
die() { echo "scripts/tarball.sh: $*" >&2; exit 1; }
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "no version in Cargo.toml"
scripts/cargo build --release --locked >&2 || die "build failed"
name="claude-usage-$version-x86_64-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -m 755 "$stage/$name"
install -m 755 target/release/claude-usage install.sh "$stage/$name/"
install -m 644 README.md CHANGELOG.md LICENSE "$stage/$name/"
(cd "$stage/$name" && sha256sum claude-usage install.sh > SHA256SUMS && chmod 644 SHA256SUMS)
mkdir -p "$OUT"
when=$(git log -1 --format=%ct 2>/dev/null || echo 0)
tar -C "$stage" --sort=name --mtime="@$when" --owner=0 --group=0 --numeric-owner \
	--format=gnu -cf - "$name" | gzip -n -9 > "$OUT/$name.tar.gz"
echo "$OUT/$name.tar.gz"
