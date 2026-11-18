#!/usr/bin/env bash
# One call to the usage endpoint, to record the shape of its answer: the HTTP status, the
# response's header names, and the body's key paths with each value's JSON type. No value is
# ever printed or stored, and a key that is not letters and underscores only (an id, an email)
# shows as <key>.
# A one-off for the maintainer (design, section 13); claude-usage itself never records this.
#   scripts/key-names.sh          says what it would do, calls nothing
#   scripts/key-names.sh --go     makes the one call
# The token: read from Claude Code's .credentials.json (only read, never written) by jq on
# stdin, held in a shell variable, and handed to curl on stdin (-H @-): never on a command line,
# in the environment or in a file. curl: /usr/bin/curl only, with an empty environment and -q
# (no .curlrc), as claude-usage runs it. It refuses to call near :00 and :30 (claude-usage's
# timer), with an expired token, or when claude-usage last saw a 429.
set -euo pipefail
# Every program from the system folders only: a jq or env found on the caller's PATH would see
# the credentials or curl's stdin.
export PATH=/usr/bin:/bin
die() { echo "scripts/key-names.sh: $*" >&2; exit 1; }
URL=https://api.anthropic.com/api/oauth/usage
CURL=/usr/bin/curl
go=0
case ${1:-} in
--go) go=1 ;;
'') ;;
*) die "usage: scripts/key-names.sh [--go]" ;;
esac
[ "$(id -u)" != 0 ] || die "not as root"
for c in jq "$CURL"; do command -v "$c" > /dev/null || die "$c not found"; done
cfg=${CLAUDE_CONFIG_DIR:-$HOME/.claude}
creds=$cfg/.credentials.json
[ -f "$creds" ] && [ ! -L "$creds" ] && [ "$(stat -c %u "$creds")" = "$(id -u)" ] ||
	die "$creds: not a file of yours"

now=$(date +%s)
exp=$(jq -r '.claudeAiOauth.expiresAt // 0 | floor' < "$creds") || die "$creds: no expiresAt number"
[[ $exp =~ ^[0-9]+$ ]] || die "$creds: expiresAt is not a number"
left=$((exp / 1000 - now))
[ "$left" -gt 120 ] || die "the token expires in ${left} s: start a Claude Code session first (no call)"
if [ -x "$HOME/.local/bin/claude-usage" ]; then
	err=$("$HOME/.local/bin/claude-usage" get 2> /dev/null | jq -r '.error // ""') || err=''
	[ "$err" != rate_limited ] || die "claude-usage last saw a 429: try again later (no call)"
fi
sec=$((10#$(date +%S) % 30))
[ "$sec" -ge 10 ] && [ "$sec" -le 22 ] ||
	die "run it between :10-:22 or :40-:52 of a minute, away from the timer's :00/:30 (no call)"

if [ "$go" = 0 ]; then
	echo "would call $URL once, with the token from $creds (expires in $((left / 60)) min)"
	echo "run again with --go to make the call"
	exit 0
fi

tok=$(jq -r '.claudeAiOauth.accessToken // ""' < "$creds")
[[ $tok =~ ^[A-Za-z0-9._~+/=-]{1,4096}$ ]] || { tok=''; die "$creds: no usable accessToken"; }
# printf is a builtin: the token is in no process's arguments.
out=$(printf 'Authorization: Bearer %s\n' "$tok" |
	/usr/bin/env -i LANG=C PATH=/usr/bin:/bin "$CURL" -q -sS --max-time 10 --connect-timeout 5 \
		--proto =https --max-redirs 0 -H @- -H 'anthropic-beta: oauth-2025-04-20' \
		-D - -o - "$URL") || { rc=$?; tok=''; die "curl failed (exit $rc)"; }
tok=''

head=${out%%$'\r\n\r\n'*}
body=${out#*$'\r\n\r\n'}
[ "$head" != "$out" ] || body=''
echo "status: $(head -n 1 <<< "$head" | tr -d '\r' | cut -d' ' -f2)"
echo "header names:"
tail -n +2 <<< "$head" | tr -d '\r' | cut -d: -f1 | tr '[:upper:]' '[:lower:]' | sort -u | sed 's/^/  /'
echo "body: ${#body} bytes"
# shellcheck disable=SC2016 # jq's own variables
jq -r '
	def word: if type == "number" then "[]"
		elif test("^[A-Za-z_]{1,40}$") then .
		else "<key>" end;
	[paths(true) as $p | ($p | map(word) | join(".")) + "  " + (getpath($p) | type)]
	| unique | .[] | "  " + .
' <<< "$body" 2> /dev/null || echo "  (not JSON)"
