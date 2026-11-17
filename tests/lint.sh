#!/usr/bin/env bash
# Lint: rustfmt and clippy (through scripts/cargo, in a container), bash syntax and shellcheck
# of every tracked shell script, and the privacy check of every tracked file and its name.
# The shellcheck used is the installed one, else koalaman/shellcheck v0.10.0 pinned by digest, in
# a container with no network and the repo read-only (`docker image rm` it to free the space).
# SC2015 (A && B || C) is allowed, as in Agent-Session-Control, whose privacy checker is used here.
# Each check prints "  ok    NAME" or "  FAIL  NAME" (then what it said, indented).
# Exit 0 only when none failed.
#   tests/lint.sh
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2
SHELLCHECK_IMG=koalaman/shellcheck@sha256:2097951f02e735b613f4a34de20c40f937a6c8f18ecb170612c88c34517221fb # v0.10.0
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
fail=0
check() {
	local name=$1
	shift
	if "$@" > "$tmp/out" 2>&1; then
		echo "  ok    $name"
	else
		echo "  FAIL  $name"
		head -n 60 "$tmp/out" | sed 's/^/        /'
		fail=$((fail + 1))
	fi
}

# Tracked shell scripts: a .sh name, or a bash/sh shebang (scripts/cargo, the git hooks).
# Each as ./NAME, so a name that starts with "-" is never read as an option.
scripts=()
while IFS= read -r -d '' f; do
	f=./$f
	[ -f "$f" ] && ! [ -L "$f" ] || continue
	case $f in
	*.sh) scripts+=("$f") ;;
	*) head -n 1 "$f" | grep -qE '^#!.*\b(bash|sh)\b' && scripts+=("$f") ;;
	esac
done < <(git ls-files -z)
[ ${#scripts[@]} -gt 0 ] || { echo "lint: no tracked shell scripts found (not a git checkout?)" >&2; exit 2; }
bash_syntax() { local f; for f in "${scripts[@]}"; do bash -n "$f" || return 1; done; }
shellcheck_all() {
	if command -v shellcheck > /dev/null; then
		shellcheck -x -s bash -e SC2015 "${scripts[@]}"
	else
		docker run --rm --network none --mount "type=bind,src=$PWD,dst=/mnt,readonly" -w /mnt \
			"$SHELLCHECK_IMG" -x -s bash -e SC2015 "${scripts[@]}"
	fi
}

check "cargo fmt" scripts/cargo fmt --check
check "cargo clippy" scripts/cargo clippy --all-targets --locked -- -D warnings
check "bash syntax" bash_syntax
check "shellcheck" shellcheck_all
check "privacy" tools/privacy-check.sh --tree

echo "lint: $fail failed"
[ "$fail" = 0 ]
