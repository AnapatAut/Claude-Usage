#!/usr/bin/env bash
# Black-box tests of claude-usage: the contract (get's JSON), the fetch rules and the token
# handling, against a fake curl (tests/fake-curl/curl) and throwaway config folders.
#
#   tests/cli.sh                 build the release binary, then run every case in a container
#   CU_ONLY=a,b tests/cli.sh     only those cases (names: the case_ functions below, no prefix)
#
# It always runs inside a container: ubuntu:24.04 pinned by digest, plus jq and procps (image
# cu-test:1, made once with network by `docker commit`, labelled). No network, no real curl, no
# real home: only tests/ and the binary are mounted, read-only. A bug in claude-usage that
# ignored HOME or CLAUDE_CONFIG_DIR could therefore never reach a real token or the real
# endpoint. Inside, it refuses to start unless HOME and CLAUDE_CONFIG_DIR are in its own temp
# folder, and the fake curl refuses every token that is not SECRET-TOKEN-XXX.
#
# Each check prints "  ok    NAME" or "  FAIL  NAME   [condition]", the same fixed name every
# run. Exit 0 only when all passed; 2 when the harness itself could not run.
#
# Each check is a single-quoted condition that ok() evals later, so no expansion is meant
# there, and variables such as rc are read only inside those conditions.
# shellcheck disable=SC2016,SC2034
set -uo pipefail

# ---------------------------------------------------------------- outside: the container
if [ "${CU_IN_CONTAINER:-}" != 1 ]; then
	REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
	UBUNTU=ubuntu@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8 # ubuntu:24.04
	IMG=cu-test:1
	die() { echo "tests/cli.sh: $*" >&2; exit 2; }
	command -v docker > /dev/null || die "docker not found"
	"$REPO/scripts/cargo" build --release --locked >&2 || die "build failed"
	BIN=$REPO/target/release/claude-usage
	[ -f "$BIN" ] && [ ! -L "$BIN" ] || die "no release binary at $BIN"
	LOCKDIR=${XDG_RUNTIME_DIR:-$HOME/.cache}
	mkdir -p "$LOCKDIR"
	exec 9> "$LOCKDIR/cu-test.lock"
	flock 9
	if docker image inspect "$IMG" > /dev/null 2>&1; then
		[ "$(docker image inspect -f '{{index .Config.Labels "cu.base"}}' "$IMG")" = "$UBUNTU" ] ||
			die "an image $IMG exists that this script did not make: remove it (docker image rm $IMG)"
	else
		setup=cu-test-setup-$$
		pulled=''
		docker image inspect "$UBUNTU" > /dev/null 2>&1 || pulled=1
		trap 'docker rm -f "$setup" >/dev/null 2>&1; [ -z "$pulled" ] || docker image rm "$UBUNTU" >/dev/null 2>&1' EXIT
		echo "tests/cli.sh: making $IMG (once; needs network)" >&2
		docker run --name "$setup" "$UBUNTU" sh -c \
			'apt-get update -qq && apt-get install -y -qq --no-install-recommends jq procps > /dev/null && rm -rf /var/lib/apt/lists/*' >&2 ||
			die "image setup failed"
		docker commit --change "LABEL cu.base=$UBUNTU" --change 'CMD ["bash"]' "$setup" "$IMG" > /dev/null
		docker rm "$setup" > /dev/null
		[ -z "$pulled" ] || docker image rm "$UBUNTU" > /dev/null
		trap - EXIT
	fi
	flock -u 9
	exec docker run --rm --init --pull never --network none --user "$(id -u):$(id -g)" \
		--memory 1g --pids-limit 512 --cap-drop ALL --security-opt no-new-privileges --read-only \
		--tmpfs /tmp:size=512m,exec \
		--mount "type=bind,src=$REPO/tests,dst=/src/tests,readonly" \
		--mount "type=bind,src=$BIN,dst=/opt/cu/claude-usage,readonly" \
		-e CU_IN_CONTAINER=1 -e CU_ONLY="${CU_ONLY:-}" \
		"$IMG" bash /src/tests/cli.sh
fi

# ---------------------------------------------------------------- inside: the harness
die() { echo "tests/cli.sh: setup: $*" >&2; exit 2; }
[ -f /.dockerenv ] || die "not in a container: refusing to run"
[ ! -e /usr/bin/curl ] && [ ! -e /bin/curl ] && [ ! -e /usr/local/bin/curl ] || die "a real curl is here"
[ "$(ls /sys/class/net)" = lo ] || die "the container has a network"
for c in jq sha256sum realpath setsid timeout pgrep; do command -v "$c" > /dev/null || die "$c missing"; done
CU=/opt/cu/claude-usage
[ -x "$CU" ] || die "no binary at $CU"
T=$(mktemp -d /tmp/cu.XXXXXX) || die "mktemp"
export HOME=$T/home CLAUDE_CONFIG_DIR=$T/creds/cfg
unset XDG_STATE_HOME XDG_CONFIG_HOME
case $HOME:$CLAUDE_CONFIG_DIR in "$T"/*:"$T"/*) ;; *) die "HOME or CLAUDE_CONFIG_DIR outside $T" ;; esac
mkdir -p "$T/bin" "$T/decoy" "$T/leaks"
cp /src/tests/fake-curl/curl "$T/bin/curl" && chmod 755 "$T/bin/curl" || die "fake curl"
printf '#!/bin/sh\ntouch "%s/decoy-ran"\nexit 1\n' "$T" > "$T/decoy/curl" && chmod 755 "$T/decoy/curl"
STDERR=$T/leaks/stderr.all
: > "$STDERR"

fail=0 pass=0
ok() {
	if eval "$2"; then
		echo "  ok    $1"
		pass=$((pass + 1))
	else
		echo "  FAIL  $1   [$2]"
		fail=$((fail + 1))
	fi
}

# The binary, its stderr kept for the leak sweep (and shown on a FAIL by the case output).
cu() { "$CU" "$@" 2>> "$STDERR"; }
G='' GRC=''
getj() {
	G=$("$CU" get 2>> "$STDERR")
	GRC=$?
}
jqg() { printf '%s' "$G" | jq -e "$1" > /dev/null 2>&1; }
F=$T/fake
calls() { if [ -f "$F/calls" ]; then wc -l < "$F/calls"; else echo 0; fi; }
lastcall() { tail -n 1 "$F/calls" 2> /dev/null; }
mode() { echo "$1" > "$F/mode"; }
wait_calls() { # until there are $1 calls, at most $2 seconds
	local i
	for ((i = 0; i < $2 * 10; i++)); do [ "$(calls)" -ge "$1" ] && return 0; sleep 0.1; done
	return 1
}

# Credentials: cred TOKEN [EXPIRES_MS [MCP]] writes a new file (new inode, like Claude Code's
# atomic save); cred_inplace rewrites the same inode.
creds_json() {
	local exp=${2:-$((($(date +%s) + 6 * 3600) * 1000))} mcp=${3:-SECRET-MCP-ONE}
	printf '{"claudeAiOauth":{"accessToken":"%s","refreshToken":"SECRET-REFRESH-%s","expiresAt":%s,"scopes":["user:inference"],"subscriptionType":"max"},"mcpOAuth":{"srv|1":{"accessToken":"%s"}}}' \
		"$1" "${1#SECRET-TOKEN-}" "$exp" "$mcp"
}
cred() {
	mkdir -p "$CLAUDE_CONFIG_DIR"
	creds_json "$@" > "$CLAUDE_CONFIG_DIR/.cred.tmp" && chmod 600 "$CLAUDE_CONFIG_DIR/.cred.tmp" &&
		mv -f "$CLAUDE_CONFIG_DIR/.cred.tmp" "$CLAUDE_CONFIG_DIR/.credentials.json"
}
cred_inplace() { creds_json "$@" > "$CLAUDE_CONFIG_DIR/.credentials.json"; }

# The state folder for the current CLAUDE_CONFIG_DIR, and an edit of its state file.
SD() { echo "$HOME/.local/state/claude-usage/$(printf '%s' "$(realpath -m "$CLAUDE_CONFIG_DIR")" | sha256sum | cut -c1-16)"; }
state() { jq -r "$1" "$(SD)/usage.json" 2> /dev/null; }
state_edit() {
	local f
	f=$(SD)/usage.json
	jq "$1" "$f" > "$T/st.tmp" && chmod 600 "$T/st.tmp" && mv -f "$T/st.tmp" "$f"
}
# A fetch is due at once after this: the last attempt moved back past a :00/:30 boundary.
age() { state_edit ".last_attempt -= ${1:-40} | .next_attempt = null"; }

# Wait until early in a 30 s window, so that a case's own steps share one window.
early() { while [ $(($(date +%s) % 30)) -lt 1 ] || [ $(($(date +%s) % 30)) -gt 18 ]; do sleep 0.5; done; }

# Before each case: sweep the last case's files for leaks, then a fresh home, config folder and
# fake. The access token may appear only in the credentials and the fake's own stdin and call
# log; the refresh token and the MCP token nowhere outside the credentials.
total_calls=0
sweep() {
	[ -d "$T/home" ] || return 0
	total_calls=$((total_calls + $(calls)))
	grep -rlE 'SECRET-(REFRESH|MCP)' "$T" --exclude-dir=creds --exclude-dir=leaks 2> /dev/null >> "$T/leaks/found"
	grep -rl 'SECRET-TOKEN' "$T" --exclude-dir=creds --exclude-dir=leaks --exclude='stdin.*' --exclude=calls 2> /dev/null |
		grep -v "^$T/bin/" >> "$T/leaks/found"
	grep -l 'SECRET' "$STDERR" >> "$T/leaks/found"
}
reset() {
	sweep
	rm -rf "$T/home" "$T/creds" "$F" "$T/decoy-ran" "$T/xdg"
	export CLAUDE_CONFIG_DIR=$T/creds/cfg
	unset XDG_STATE_HOME SSLKEYLOGFILE https_proxy HTTPS_PROXY CURL_CA_BUNDLE
	mkdir -p "$HOME/.config/claude-usage" "$F"
	printf 'curl = %s/bin/curl\n' "$T" > "$HOME/.config/claude-usage/config"
	chmod 600 "$HOME/.config/claude-usage/config"
	chmod 700 "$HOME" "$HOME/.config" "$HOME/.config/claude-usage"
	cred SECRET-TOKEN-AAA
	mode ok
}

# ---------------------------------------------------------------- cases
case_contract() {
	reset
	getj
	ok "get exits 0 with no state" '[ "$GRC" = 0 ] && jqg ".schema == 0"'
	ok "empty result: exact shape" 'jqg ". == {\"schema\":0,\"fetched_at\":null,\"error\":null,\"windows\":{\"five_hour\":null,\"seven_day\":null}}"'
	ok "get creates nothing" '[ "$GRC" = 0 ] && [ ! -e "$HOME/.local/state/claude-usage" ]'
	"$CU" get 2>> "$STDERR" | head -c 1 > /dev/null
	rc=${PIPESTATUS[0]}
	ok "get into a closed pipe exits 0" '[ "$rc" = 0 ]'
	mkdir -p "$(SD)" && chmod 700 "$(SD)"
	printf 'garbage{' > "$(SD)/usage.json" && chmod 600 "$(SD)/usage.json"
	getj
	ok "garbled state: clean empty result" '[ "$GRC" = 0 ] && jqg ".fetched_at == null and .windows.five_hour == null and .schema == 0"'
	printf '{"state_version":99}' > "$(SD)/usage.json"
	getj
	ok "future state version: clean empty result" '[ "$GRC" = 0 ] && jqg ".fetched_at == null and .windows.seven_day == null"'
	"$CU" get --refresh > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "get --refresh prints the same contract" '[ "$rc" = 0 ] && jq -e ".schema == 0 and (.windows | has(\"five_hour\"))" "$T/out" > /dev/null'
}

case_fetch_success() {
	reset
	early
	cu fetch
	rc=$?
	ok "fetch exits 0" '[ "$rc" = 0 ]'
	ok "one call" '[ "$(calls)" = 1 ]'
	ok "token on curl's stdin, one line" '[ "$(cat "$F/stdin.1")" = "Authorization: Bearer SECRET-TOKEN-AAA" ] && [ "$(wc -l < "$F/stdin.1")" = 1 ]'
	ok "token not in curl's argv" '[ -s "$F/argv.1" ] && ! grep -q SECRET "$F/argv.1"'
	ok "curl argv: -q first" '[ "$(head -n 1 "$F/argv.1")" = -q ]'
	ok "curl argv: https only, no redirects" 'grep -qx -- "=https" "$F/argv.1" && grep -A1 -x -- "--max-redirs" "$F/argv.1" | grep -qx 0'
	ok "curl argv: the usage endpoint and beta header" 'grep -qx "https://api.anthropic.com/api/oauth/usage" "$F/argv.1" && grep -qx "anthropic-beta: oauth-2025-04-20" "$F/argv.1"'
	ok "curl env: only PATH and LANG" '[ "$(cut -d= -f1 "$F/env.1" | sort | tr "\n" " ")" = "LANG PATH " ]'
	ok "curl runs in /" '[ "$(cat "$F/cwd.1")" = / ]'
	ok "token not in claude-usage's own argv" '[ -s "$F/parent-cmdline.1" ] && ! grep -q SECRET "$F/parent-cmdline.1"'
	getj
	ok "numbers shown" 'jqg ".error == null and .windows.five_hour.pct == 30.4 and .windows.seven_day.pct == 12"'
	ok "pct kept as sent (12, not 12.0)" 'printf "%s" "$G" | tr -d " \n" | grep -q "\"pct\":12[,}]"'
	ok "unknown windows not shown" 'jqg "(.windows | keys) == [\"five_hour\",\"seven_day\"]"'
	ok "resets_at in Unix seconds" 'jqg ".windows.five_hour.resets_at > now and .windows.five_hour.resets_at < now + 3*3600"'
	ok "fetched_at is now" 'jqg "(.fetched_at - now | fabs) < 30"'
	ok "state folder 700, state file 600" '[ "$(stat -c %a "$(SD)")" = 700 ] && [ "$(stat -c %a "$(SD)/usage.json")" = 600 ]'
	ok "no temp files left" '[ -f "$(SD)/usage.json" ] && [ -z "$(ls -A "$(SD)" | grep -vx -e usage.json -e .lock)" ]'
	ok "nothing written in the config folder" '[ "$(calls)" = 1 ] && [ "$(ls -A "$CLAUDE_CONFIG_DIR")" = .credentials.json ]'
	ok "state records the config folder" '[ "$(state .config_dir)" = "$(realpath "$CLAUDE_CONFIG_DIR")" ]'
	cu fetch
	cu fetch
	ok "same window: no second call" '[ "$(calls)" = 1 ] && [ "$(state .last_attempt)" -gt 0 ]'
}

case_rate_limited() {
	reset
	early
	cu fetch
	mode r429
	echo 120 > "$F/retry_after"
	age
	cu fetch
	getj
	ok "429: one call, rate_limited, old numbers kept" '[ "$(calls)" = 2 ] && jqg ".error == \"rate_limited\" and .windows.five_hour.pct == 30.4"'
	ok "429: Retry-After honoured" '[ "$(state ".next_attempt - .last_attempt")" -ge 120 ] && [ "$(state .retry_after_seen)" = true ]'
	age 40
	state_edit '.next_attempt = (now | floor) + 100'
	cu fetch
	ok "429: no call while backing off" '[ "$(calls)" = 2 ] && [ "$(state .last_error)" = rate_limited ]'
	state_edit '.next_attempt = (now | floor) - 1 | .last_attempt -= 200'
	mode ok
	cu fetch
	getj
	ok "after the backoff: one call, numbers fresh" '[ "$(calls)" = 3 ] && jqg ".error == null"'
	mode r429
	rm -f "$F/retry_after"
	age 200
	cu fetch
	ok "429 without Retry-After: backoff starts at 60 s" '[ "$(calls)" = 4 ] && [ "$(state ".next_attempt - .last_attempt")" -ge 60 ] && [ "$(state ".next_attempt - .last_attempt")" -le 600 ]'
}

case_auth_errors() {
	reset
	early
	mode r401
	cu fetch
	getj
	ok "401: unauthorized" '[ "$(calls)" = 1 ] && jqg ".error == \"unauthorized\""'
	age
	cu fetch
	ok "401: no retry with the same credentials" '[ "$(calls)" = 1 ] && [ "$(state .last_error)" = unauthorized ]'
	mode ok
	cred SECRET-TOKEN-BBB
	cu fetch
	ok "401: new credentials retried at once" '[ "$(calls)" = 2 ] && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'
	reset
	mode r403
	cu fetch
	getj
	ok "403: forbidden" '[ "$(calls)" = 1 ] && jqg ".error == \"forbidden\""'
}

case_http_errors() {
	reset
	early
	mode r500
	cu fetch
	getj
	ok "500: http_error, next boundary" '[ "$(calls)" = 1 ] && jqg ".error == \"http_error\""'
	reset
	mode r404
	cu fetch
	ok "404: http_error, backoff from 60 s" '[ "$(calls)" = 1 ] && [ "$(state .last_error)" = http_error ] && [ "$(state ".next_attempt - .last_attempt")" -ge 60 ]'
	reset
	mode html
	cu fetch
	getj
	ok "HTML with 200: bad_response" '[ "$(calls)" = 1 ] && jqg ".error == \"bad_response\""'
	reset
	mode huge
	cu fetch
	getj
	ok "a body over 64 KiB: bad_response" '[ "$(calls)" = 1 ] && jqg ".error == \"bad_response\""'
	reset
	mode oldcurl
	cu fetch
	getj
	ok "curl older than 7.84: no_curl, not bad_response" '[ "$(calls)" = 1 ] && jqg ".error == \"no_curl\""'
	reset
	mode partial
	cu fetch
	getj
	ok "curl fails part way: network" '[ "$(calls)" = 1 ] && jqg ".error == \"network\""'
	reset
	mode hang
	s=$(date +%s)
	timeout 40 "$CU" fetch 2>> "$STDERR"
	e=$(($(date +%s) - s))
	getj
	ok "a hung curl is killed by the watchdog" '[ "$(calls)" = 1 ] && [ "$e" -le 15 ] && jqg ".error == \"network\""'
	reset
	mode nostdin
	cu fetch
	getj
	ok "a curl that never reads stdin: fetch finishes" '[ "$(calls)" = 1 ] && jqg ".error == null"'
}

case_credentials() {
	reset
	cred SECRET-TOKEN-AAA $((($(date +%s) - 3600) * 1000))
	cu fetch
	getj
	ok "expired token: no call, token_expired" '[ "$(calls)" = 0 ] && jqg ".error == \"token_expired\""'
	reset
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "no credentials file: no call, no_credentials" '[ "$(calls)" = 0 ] && jqg ".error == \"no_credentials\" and .windows.five_hour == null"'
	reset
	printf '{"mcpOAuth":{}}' > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "no claudeAiOauth (API-key setup): no_credentials" '[ "$(calls)" = 0 ] && jqg ".error == \"no_credentials\""'
	reset
	cred 'SECRET-TOKEN-AAA\r\nX-Injected: 1'
	cu fetch
	getj
	ok "token with CR/LF: no call, bad_credentials" '[ "$(calls)" = 0 ] && jqg ".error == \"bad_credentials\""'
	reset
	printf '{"claudeAiOauth":' > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "unparsable credentials: bad_credentials, nothing echoed" '[ "$(calls)" = 0 ] && jqg ".error == \"bad_credentials\""'
	reset
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	mkfifo "$CLAUDE_CONFIG_DIR/.credentials.json"
	s=$(date +%s)
	timeout 20 "$CU" fetch 2>> "$STDERR"
	e=$(($(date +%s) - s))
	getj
	ok "a FIFO as credentials: no hang, bad_credentials" '[ "$e" -le 5 ] && jqg ".error == \"bad_credentials\""'
	reset
	early
	mkdir -p "$T/creds/dot"
	mv "$CLAUDE_CONFIG_DIR/.credentials.json" "$T/creds/dot/c.json"
	ln -s "$T/creds/dot/c.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	ok "symlinked credentials are followed" '[ "$(calls)" = 1 ] && [ "$(lastcall)" = SECRET-TOKEN-AAA ]'
	creds_json SECRET-TOKEN-BBB > "$T/creds/dot/n.json" && mv -f "$T/creds/dot/n.json" "$T/creds/dot/c.json"
	cu fetch
	ok "a switch behind the symlink is seen" '[ "$(calls)" = 2 ] && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'
}

case_accounts() {
	reset
	early
	cu fetch
	sleep 1
	cred SECRET-TOKEN-BBB
	getj
	ok "switch: old numbers hidden at once" '[ "$(calls)" = 1 ] && jqg ".error == \"account_changed\" and .windows.five_hour == null"'
	cu fetch
	getj
	ok "switch: one call with the new token" '[ "$(calls)" = 2 ] && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'
	ok "switch: the new account's numbers" 'jqg ".error == null and .windows.five_hour.pct == 80"'

	reset
	early
	cu fetch
	sleep 1
	cred SECRET-TOKEN-BBB
	mode r429
	cu fetch
	getj
	ok "switch then 429: old numbers never shown" '[ "$(calls)" = 2 ] && jqg ".windows.five_hour == null and .error == \"account_changed\""'

	reset
	early
	cu fetch
	sleep 1
	cred SECRET-TOKEN-BBB
	mode hang
	"$CU" fetch 2>> "$STDERR" &
	p=$!
	wait_calls 2 10
	kill -9 "$p"
	wait "$p" 2> /dev/null
	getj
	ok "switch then SIGKILL mid-call: old numbers never shown" '[ "$(calls)" = 2 ] && jqg ".windows.five_hour == null"'
	mode ok
	cu fetch
	ok "a killed fetch still counts as an attempt" '[ "$(calls)" = 2 ] && [ "$(state .attempt_stamp)" != null ]'

	reset
	early
	cp -p "$CLAUDE_CONFIG_DIR/.credentials.json" "$T/creds/saved-a.json"
	sleep 1
	cu fetch
	cred SECRET-TOKEN-BBB
	cu fetch
	cp -p "$T/creds/saved-a.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
	getj
	ok "restore with cp -p: hidden until confirmed" '[ "$(calls)" = 2 ] && jqg ".error == \"account_changed\""'
	cu fetch
	getj
	ok "restore with cp -p: refetched with the restored token" '[ "$(calls)" = 3 ] && [ "$(lastcall)" = SECRET-TOKEN-AAA ] && jqg ".windows.five_hour.pct == 30.4"'

	reset
	early
	cu fetch
	sleep 1
	cred_inplace SECRET-TOKEN-CCC
	cu fetch
	getj
	ok "in-place token refresh: one call" '[ "$(calls)" = 2 ] && [ "$(lastcall)" = SECRET-TOKEN-CCC ] && jqg ".windows.five_hour.pct == 55"'

	reset
	early
	cu fetch
	sleep 1
	cred_inplace SECRET-TOKEN-AAA "" SECRET-MCP-TWO
	cu fetch
	getj
	ok "MCP-only change: no call, numbers back" '[ "$(calls)" = 1 ] && jqg ".error == null and .windows.five_hour.pct == 30.4"'
	chmod 400 "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "chmod-only change: no call, numbers back" '[ "$(calls)" = 1 ] && jqg ".error == null and .windows.five_hour.pct == 30.4"'

	reset
	early
	cu fetch
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	getj
	ok "logout: numbers hidden" '[ "$(calls)" = 1 ] && jqg ".error == \"no_credentials\" and .windows.five_hour == null"'

	reset
	early
	creds_json SECRET-TOKEN-BBB > "$T/creds/next.json"
	printf '#!/bin/sh\nmv -f "%s" "%s"\nrm -f "%s"\n' "$T/creds/next.json" "$CLAUDE_CONFIG_DIR/.credentials.json" "$F/during" > "$F/during"
	chmod 755 "$F/during"
	cu fetch
	getj
	ok "switch during a fetch: its numbers are hidden" '[ "$(calls)" = 1 ] && jqg ".windows.five_hour == null and .error == \"account_changed\""'
	cu fetch
	getj
	ok "switch during a fetch: refetched with the new token" '[ "$(calls)" = 2 ] && [ "$(lastcall)" = SECRET-TOKEN-BBB ] && jqg ".windows.five_hour.pct == 80"'
}

case_config() {
	reset
	CLAUDE_CONFIG_DIR=relative/dir "$CU" get > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "relative CLAUDE_CONFIG_DIR: bad_config, exit 0" '[ "$rc" = 0 ] && jq -e ".error == \"bad_config\"" "$T/out" > /dev/null'
	reset
	mkdir -p "$HOME/.claude" && mv "$CLAUDE_CONFIG_DIR/.credentials.json" "$HOME/.claude/"
	CLAUDE_CONFIG_DIR='' "$CU" fetch 2>> "$STDERR"
	ok "empty CLAUDE_CONFIG_DIR means ~/.claude" '[ "$(calls)" = 1 ]'
	mv "$HOME/.claude/.credentials.json" "$CLAUDE_CONFIG_DIR/"
	rm -rf "$HOME/.claude"
	reset
	export XDG_STATE_HOME=$T/xdg
	cu fetch
	ok "XDG_STATE_HOME is ignored" '[ "$(calls)" = 1 ] && [ -f "$(SD)/usage.json" ] && [ ! -e "$T/xdg" ]'
	unset XDG_STATE_HOME
	ln -s "$CLAUDE_CONFIG_DIR" "$T/creds/link"
	CLAUDE_CONFIG_DIR=$T/creds/link "$CU" get > "$T/out" 2>> "$STDERR"
	ok "two spellings of one folder share one state" 'jq -e ".windows.five_hour.pct == 30.4" "$T/out" > /dev/null'
	CLAUDE_CONFIG_DIR=$T/creds/missing "$CU" get > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "a missing config folder: no_credentials, exit 0" '[ "$rc" = 0 ] && jq -e ".error == \"no_credentials\"" "$T/out" > /dev/null'
	reset
	mkdir -p "$HOME/.local/state/claude-usage" "$T/elsewhere"
	ln -s "$T/elsewhere" "$(SD)"
	cu fetch
	rc=$?
	getj
	ok "a symlinked state folder is refused" '[ "$rc" = 1 ] && [ -z "$(ls -A "$T/elsewhere")" ] && jqg ".error == \"state_unusable\""'
	reset
	mkdir -p "$(SD)" && chmod 500 "$(SD)"
	cu fetch
	rc=$?
	getj
	ok "an unwritable state folder: no call, exit 1" '[ "$rc" = 1 ] && [ "$(calls)" = 0 ] && jqg ".error == \"state_unusable\""'
	chmod 700 "$(SD)"
	reset
	chmod 620 "$HOME/.config/claude-usage/config"
	cu fetch
	getj
	ok "a group-writable config file is ignored" '[ "$(calls)" = 0 ] && jqg ".error == \"no_curl\""'
}

case_curl_choice() {
	reset
	rm -f "$HOME/.config/claude-usage/config"
	PATH=$T/decoy:.:$PATH "$CU" fetch 2>> "$STDERR"
	getj
	ok "curl is never taken from PATH" '[ ! -e "$T/decoy-ran" ] && jqg ".error == \"no_curl\""'
	reset
	export SSLKEYLOGFILE=$T/keys https_proxy=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 CURL_CA_BUNDLE=$T/ca
	cu fetch
	ok "SSLKEYLOGFILE, proxies and CA bundle not passed to curl" '[ "$(calls)" = 1 ] && ! grep -qiE "^(SSLKEYLOGFILE|https_proxy|CURL_CA_BUNDLE)=" "$F/env.1"'
	unset SSLKEYLOGFILE https_proxy HTTPS_PROXY CURL_CA_BUNDLE
}

case_refresh() {
	reset
	s=$(date +%s%N)
	"$CU" get --refresh > "$T/out" 2>> "$STDERR"
	rc=$?
	e=$((($(date +%s%N) - s) / 1000000))
	ok "get --refresh returns at once" '[ "$rc" = 0 ] && [ "$e" -lt 1000 ] && jq -e ".schema == 0" "$T/out" > /dev/null'
	wait_calls 1 10
	ok "get --refresh starts one fetch" '[ "$(calls)" = 1 ] && [ -f "$(SD)/usage.json" ]'
	reset
	mode slow
	s=$(date +%s%N)
	"$CU" get --refresh 2>&1 3>&1 | cat > /dev/null
	e=$((($(date +%s%N) - s) / 1000000))
	wait_calls 1 10
	ok "the detached fetch holds no pipe of the reader (stdout, stderr, fd 3)" '[ "$e" -lt 1500 ] && [ "$(calls)" = 1 ]'
	"$CU" get --refresh > /dev/null 2>> "$STDERR"
	"$CU" get --refresh > /dev/null 2>> "$STDERR"
	sleep 4
	ok "none started while one runs" '[ "$(calls)" = 1 ] && [ -f "$(SD)/usage.json" ]'
	reset
	mode slow
	setsid bash -c '"$0" get --refresh > /dev/null 2>&1; sleep 30' "$CU" &
	p=$!
	sleep 0.5
	kill -TERM -- "-$p" 2> /dev/null
	wait_calls 1 10
	sleep 4
	getj
	ok "the fetch survives a kill of the reader's process group" '[ "$(calls)" = 1 ] && jqg ".windows.five_hour.pct == 30.4"'
	reset
	mkdir -p "$T/creds/other"
	creds_json SECRET-TOKEN-CCC > "$T/creds/other/.credentials.json"
	CLAUDE_CONFIG_DIR=$T/creds/other "$CU" get --refresh > /dev/null 2>> "$STDERR"
	wait_calls 1 10
	ok "the detached fetch uses the reader's config folder" '[ "$(lastcall)" = SECRET-TOKEN-CCC ]'
}

case_now() {
	reset
	early
	cu fetch
	age 11
	cu fetch --now
	rc=$?
	ok "--now: fetches before the boundary" '[ "$rc" = 0 ] && [ "$(calls)" = 2 ]'
	cu fetch --now
	rc=$?
	ok "--now within 10 s: refused, exit 3" '[ "$rc" = 3 ] && [ "$(calls)" = 2 ]'
	age 11
	for _ in $(seq 20); do "$CU" fetch --now 2>> "$STDERR" & done
	wait
	ok "20 concurrent --now: one call" '[ "$(calls)" = 3 ]'
}

case_robustness() {
	reset
	early
	mode hang
	"$CU" fetch 2>> "$STDERR" &
	p=$!
	wait_calls 1 10
	kill -9 "$p"
	wait "$p" 2> /dev/null
	mode ok
	cu fetch
	getj
	ok "SIGKILL mid-call: the next fetch does not call" '[ "$(calls)" = 1 ] && [ "$(state .last_attempt)" -gt 0 ]'
	ok "SIGKILL mid-call: get says stalled" 'jqg ".error == \"stalled\""'
}

# ---------------------------------------------------------------- run
all=(contract fetch_success rate_limited auth_errors http_errors credentials accounts config curl_choice refresh now robustness)
sel=("${all[@]}")
if [ -n "${CU_ONLY:-}" ]; then
	IFS=, read -r -a sel <<< "$CU_ONLY"
	for c in "${sel[@]}"; do [[ " ${all[*]} " == *" $c "* ]] || die "unknown case: $c"; done
fi
for c in "${sel[@]}"; do
	echo "== $c"
	"case_$c"
done
sweep
echo "== leaks"
ok "no token anywhere but the credentials and the fake's log" '[ "$total_calls" -gt 0 ] && [ ! -s "$T/leaks/found" ]'
[ -s "$T/leaks/found" ] && sed 's/^/        leak: /' "$T/leaks/found" | sort -u
pg=$(pgrep -u "$(id -u)" -f "$CU" | wc -l)
ok "no claude-usage process left running" '[ "$total_calls" -gt 0 ] && [ "$pg" = 0 ]'
echo "cli: $pass passed, $fail failed"
[ "$fail" = 0 ]
