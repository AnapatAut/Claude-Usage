#!/usr/bin/env bash
# Lint: rustfmt and clippy (through scripts/cargo, in a container), bash syntax of every tracked
# shell script, shellcheck when it is installed (CI always has it), and the privacy check of
# every tracked file and its name. Each check prints "  ok    NAME", "  FAIL  NAME" (then what it
# said, indented) or "  skip  NAME". Exit 0 only when none failed.
#   tests/lint.sh
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2
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
scripts=()
while IFS= read -r -d '' f; do
	[ -f "$f" ] && ! [ -L "$f" ] || continue
	case $f in
	*.sh) scripts+=("$f") ;;
	*) head -n 1 "$f" | grep -qE '^#!.*\b(bash|sh)\b' && scripts+=("$f") ;;
	esac
done < <(git ls-files -z)
bash_syntax() { local f; for f in "${scripts[@]}"; do bash -n "$f" || return 1; done; }

check "cargo fmt" scripts/cargo fmt --check
check "cargo clippy" scripts/cargo clippy --all-targets --locked -- -D warnings
check "bash syntax" bash_syntax
if command -v shellcheck > /dev/null; then
	check "shellcheck" shellcheck -x "${scripts[@]}"
else
	echo "  skip  shellcheck (not installed)"
fi
check "privacy" tools/privacy-check.sh --tree

echo "lint: $fail failed"
[ "$fail" = 0 ]
