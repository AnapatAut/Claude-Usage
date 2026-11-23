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
for c in sha256sum awk; do command -v "$c" > /dev/null || die "$c is missing"; done
# Each file against its own line (no -c options: BusyBox's sha256sum has none of GNU's). Like
# --strict: every line must be "<64 hex>  <name>", and each file needs exactly one.
[ -f SHA256SUMS ] || die "SHA256SUMS is missing: download the release again"
awk 'NF != 2 || length($1) != 64 || $1 !~ /^[0-9a-f]+$/ { exit 1 }' SHA256SUMS ||
	die "SHA256SUMS is malformed: download the release again"
for f in claude-usage install.sh; do
	want=$(awk -v f="$f" '$2 == f { print $1 }' SHA256SUMS)
	got=$(sha256sum < "$f") && got=${got%% *}
	[ -n "$want" ] && [ "$got" = "$want" ] || die "the files do not match SHA256SUMS: download the release again"
done
exec ./claude-usage install "$@"
}
