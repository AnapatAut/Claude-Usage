#!/bin/sh
# Installs claude-usage for you (not for every user, never as root) from this release tarball:
#   ./install.sh [--timer | --no-timer]
# Checks the files against SHA256SUMS, then runs this folder's claude-usage install, which
# places ~/.local/bin/claude-usage and the systemd user units (see README.md). Rerun it to
# upgrade or to finish an install that stopped part-way.
# One { … } group: the shell reads all of it before it runs any of it.
{
set -eu
cd -- "$(dirname -- "$0")"
die() { echo "install.sh: $*" >&2; exit 1; }
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || die "claude-usage runs on Linux x86_64 only"
[ "$(id -u)" != 0 ] || die "run it as yourself, not as root (it installs into your home)"
command -v sha256sum > /dev/null || die "sha256sum is missing"
sha256sum --quiet --strict -c SHA256SUMS || die "the files do not match SHA256SUMS: download the release again"
exec ./claude-usage install "$@"
}
