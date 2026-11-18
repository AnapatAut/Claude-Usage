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
# endpoint. The work folder is a tmpfs of the user's, mode 700 (claude-usage refuses a curl in a
# folder others can write). The fake curl refuses every token that is not SECRET-TOKEN-XXX, and
# the harness checks the fake itself before the cases run.
#
# Each check prints "  ok    NAME" or "  FAIL  NAME   [condition]", the same fixed name every
# run. Exit 0 only when all passed; 2 when the harness itself could not run.
#
# Timing: the fetch rules hang on :00/:30 wall-clock boundaries and a 5 s floor. Cases start
# early in a 30 s window (early, window) so that their steps share it, move recorded times back
# by editing the state (age, past_floor) instead of sleeping, and check "no call" with the
# floor already passed, so that the floor is never the reason a check passes.
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
	exec 9>&-
	exec docker run --rm --init --pull never --network none --user "$(id -u):$(id -g)" \
		--memory 1g --pids-limit 512 --cap-drop ALL --security-opt no-new-privileges --read-only \
		--tmpfs /tmp:size=64m \
		--tmpfs "/work:size=512m,exec,mode=0700,uid=$(id -u),gid=$(id -g)" \
		--tmpfs "/small:size=64k,mode=0700,uid=$(id -u),gid=$(id -g)" \
		--mount "type=bind,src=$REPO/tests,dst=/src/tests,readonly" \
		--mount "type=bind,src=$REPO/systemd,dst=/src/systemd,readonly" \
		--mount "type=bind,src=$REPO/install.sh,dst=/src/install.sh,readonly" \
		--mount "type=bind,src=$BIN,dst=/opt/cu/claude-usage,readonly" \
		-e CU_IN_CONTAINER=1 -e CU_ONLY="${CU_ONLY:-}" \
		"$IMG" bash /src/tests/cli.sh
fi

# ---------------------------------------------------------------- inside: the harness
die() { echo "tests/cli.sh: setup: $*" >&2; exit 2; }
[ -f /.dockerenv ] || die "not in a container: refusing to run"
[ ! -e /usr/bin/curl ] && [ ! -e /bin/curl ] && [ ! -e /usr/local/bin/curl ] || die "a real curl is here"
[ "$(ls /sys/class/net)" = lo ] || die "the container has a network"
for c in jq sha256sum realpath setsid timeout pgrep pkill flock ps stat; do
	command -v "$c" > /dev/null || die "$c missing"
done
[ "$(stat -c '%a %u' /work)" = "700 $(id -u)" ] || die "/work is not the user's own 700 folder"
[ "$(stat -c '%a %u' /small)" = "700 $(id -u)" ] || die "/small is not the user's own 700 folder"
[ "$(stat -c %u /tmp)" = 0 ] || die "/tmp is not root's (the HOME-owner case needs that)"
# The core-dump check means something only where the limit is not already 0.
ulimit -S -c "$(ulimit -H -c)"
[ "$(ulimit -c)" != 0 ] || die "the core size limit is 0 here: the core-dump check would prove nothing"
export LC_ALL=C
CU=/opt/cu/claude-usage
[ -x "$CU" ] || die "no binary at $CU"
T=$(mktemp -d /work/cu.XXXXXX) || die "mktemp"
export HOME=$T/home CLAUDE_CONFIG_DIR=$T/creds/cfg
unset XDG_STATE_HOME XDG_CONFIG_HOME
mkdir -p "$T/bin" "$T/decoy" "$T/leaks" && chmod 755 "$T/bin" || die "mkdir"
cp /src/tests/fake-curl/curl "$T/bin/curl" && chmod 755 "$T/bin/curl" || die "fake curl"
printf '#!/bin/sh\ntouch "%s/decoy-ran"\nexit 1\n' "$T" > "$T/decoy/curl" && chmod 755 "$T/decoy/curl"
STDERR=$T/leaks/stderr.all
: > "$STDERR"
F=$T/fake
EMPTY='{"schema":0,"fetched_at":null,"error":null,"windows":{"five_hour":null,"seven_day":null}}'
HIDDEN='.windows == {"five_hour":null,"seven_day":null} and .fetched_at == null'

# The fake itself, run the way claude-usage must run it: it answers a test token, refuses
# anything else without saving it, flags a wrong argument list, and sees no stray fd.
ARGV=(-q -sS --max-time 5 --connect-timeout 3 --proto '=https' --max-redirs 0 -H @-
	-H 'anthropic-beta: oauth-2025-04-20' -o - -w '\n%{http_code} %header{retry-after}'
	https://api.anthropic.com/api/oauth/usage)
fake() { (cd / && exec env -i PATH=/usr/bin:/bin LANG=C "$T/bin/curl" "$@"); }
mkdir -p "$F"
out=$(printf 'Authorization: Bearer SECRET-TOKEN-AAA\n' | fake "${ARGV[@]}") &&
	[ "${out##*$'\n'}" = "200 " ] && [ "$(cat "$F/calls")" = SECRET-TOKEN-AAA ] &&
	[ ! -s "$F/fds.1" ] && [ "$(sort "$F/env.1" | tr '\n' ' ')" = "LANG=C PATH=/usr/bin:/bin " ] ||
	die "fake curl self-test: a good call"
printf 'Authorization: Bearer not-a-test-token\n' | fake "${ARGV[@]}" > /dev/null
[ "$?" = 99 ] && [ -s "$F/refused" ] && [ ! -e "$F/stdin.2" ] || die "fake curl self-test: refusal"
printf 'Authorization: Bearer SECRET-TOKEN-AAA\r\nX: 1\n' | fake "${ARGV[@]}" > /dev/null
[ "$?" = 99 ] && [ ! -e "$F/stdin.3" ] || die "fake curl self-test: injected header"
printf 'Authorization: Bearer SECRET-TOKEN-AAA\n' | fake "${ARGV[@]:1}" > /dev/null
[ "$?" = 97 ] && [ -s "$F/argv-mismatch" ] || die "fake curl self-test: argv"
rm -rf "$F"

fail=0 pass=0 EDIT_FAIL=''
ok() {
	# A condition's own complaints (an empty value where a number is due) go to a side file.
	# After a state edit that failed, the case's premise is gone: every later check fails.
	if [ -z "$EDIT_FAIL" ] && eval "$2" 2>> "$T/leaks/conditions.err"; then
		echo "  ok    $1"
		pass=$((pass + 1))
	else
		echo "  FAIL  $1   [$2]${EDIT_FAIL:+ (a state edit before it failed)}"
		fail=$((fail + 1))
	fi
}

# The binary, its stderr kept for the leak sweep. cu_e FILE …: that run's stderr also in FILE.
cu() { "$CU" "$@" 2>> "$STDERR"; }
cu_e() {
	local f=$1 r
	shift
	"$CU" "$@" 2> "$f"
	r=$?
	cat "$f" >> "$STDERR"
	return "$r"
}
G='' GRC=''
getj() {
	G=$("$CU" get 2>> "$STDERR")
	GRC=$?
	printf '%s\n' "$G" >> "$T/gets"
}
jqg() { printf '%s' "$G" | jq -e "$1" > /dev/null 2>&1; }
jqf() { jq -e "$1" "$2" > /dev/null 2>&1; }

# The fake's records. runs counts every run (refused ones too), calls only accepted ones;
# called N means exactly N runs, all accepted.
runs() { cat "$F/n" 2> /dev/null || echo 0; }
calls() { if [ -f "$F/calls" ]; then wc -l < "$F/calls"; else echo 0; fi; }
called() { [ "$(runs)" = "$1" ] && [ "$(calls)" = "$1" ]; }
lastcall() { tail -n 1 "$F/calls" 2> /dev/null; }
mode() { echo "$1" > "$F/mode"; }
wait_for() { # COND SECONDS: until the condition holds, at most that long
	local i
	for ((i = 0; i < $2 * 10; i++)); do eval "$1" && return 0; sleep 0.1; done
	return 1
}
wait_calls() { wait_for '[ "$(calls)" -ge '"$1"' ]' "$2"; }
# How many pids (processes or threads) a command made besides its own: 0 means it spawned
# nothing. Exact, unlike sampling with pgrep: a wrongly spawned child that exits at once still
# used a pid. The container's pid namespace holds only this harness.
spawned() {
	local p0 p1
	read -r p0 < /proc/sys/kernel/ns_last_pid
	"$@" >> "$T/gets" 2>> "$STDERR"
	read -r p1 < /proc/sys/kernel/ns_last_pid
	echo $((p1 - p0 - 1))
}
[ "$(spawned /bin/true)" = 0 ] && [ "$(spawned sh -c '/bin/true & wait')" = 1 ] ||
	die "pid counting does not work here"
# End a `flock -o … sleep N &` lock holder and its sleep.
unlock() {
	pkill -P "$1" 2> /dev/null
	kill "$1" 2> /dev/null
	wait "$1" 2> /dev/null
}

# Credentials: cred TOKEN [EXPIRES_MS|none [MCP]] writes a new file (new inode, like Claude
# Code's atomic save); cred_inplace rewrites the same inode. EXP is fixed per case, so that a
# rewrite with the same token leaves the claudeAiOauth bytes the same.
EXP=0
creds_json() {
	local exp=${2:-$EXP} mcp=${3:-SECRET-MCP-ONE} e=''
	[ "$exp" = none ] || e=",\"expiresAt\":$exp"
	printf '{"claudeAiOauth":{"accessToken":"%s","refreshToken":"SECRET-REFRESH-%s"%s,"scopes":["user:inference"],"subscriptionType":"max"},"mcpOAuth":{"srv|1":{"accessToken":"%s"}}}' \
		"$1" "${1#SECRET-TOKEN-}" "$e" "$mcp"
}
cred() {
	mkdir -p "$CLAUDE_CONFIG_DIR"
	creds_json "$@" > "$CLAUDE_CONFIG_DIR/.cred.tmp" && chmod 600 "$CLAUDE_CONFIG_DIR/.cred.tmp" &&
		mv -f "$CLAUDE_CONFIG_DIR/.cred.tmp" "$CLAUDE_CONFIG_DIR/.credentials.json"
}
cred_inplace() { creds_json "$@" > "$CLAUDE_CONFIG_DIR/.credentials.json"; }
# Credentials of exactly N bytes: the test token's file with a padding key in front.
pad_creds() {
	local base rest h t n
	base=$(creds_json SECRET-TOKEN-AAA)
	rest=${base#\{}
	h='{"pad":"' t="\",$rest"
	n=$(($1 - ${#h} - ${#t}))
	printf '%s' "$h"
	head -c "$n" /dev/zero | tr '\0' A
	printf '%s' "$t"
}

# The state folder for the current CLAUDE_CONFIG_DIR, its state file, and edits of it (quiet;
# false when there is no state to edit).
SD() { echo "$HOME/.local/state/claude-usage/$(printf '%s' "$(realpath -m "$CLAUDE_CONFIG_DIR")" | sha256sum | cut -c1-16)"; }
mkstate() {
	mkdir -p "$(SD)" &&
		chmod 700 "$HOME/.local" "$HOME/.local/state" "$HOME/.local/state/claude-usage" "$(SD)"
}
state() { jq -r "$1" "$(SD)/usage.json" 2> /dev/null; }
stateq() { jq -e "$1" "$(SD)/usage.json" > /dev/null 2>&1; }
state_edit() {
	local f
	f=$(SD)/usage.json
	[ -f "$f" ] && jq "$1" "$f" > "$T/st.tmp" 2> /dev/null && chmod 600 "$T/st.tmp" &&
		mv -f "$T/st.tmp" "$f" && return 0
	EDIT_FAIL=1
	return 1
}
# Every recorded time N seconds back (last attempt, backoff end, --now times): with N >= 30 a
# boundary has passed.
age() {
	state_edit ".last_attempt -= $1
		| if .next_attempt then .next_attempt -= $1 else . end
		| if .now_times then .now_times |= map(. - $1) else . end"
}
# As age, for outcomes where the design does not say whether an attempt is recorded.
age_any() { state_edit "if .last_attempt then .last_attempt -= $1 else . end"; }
# The last attempt at least 6 s back (past the 5 s floor), and no further.
past_floor() { state_edit '.last_attempt = ([.last_attempt, (now | floor) - 6] | min)'; }
# next_attempt minus last_attempt within [LO, HI].
gap() {
	local d
	d=$(state '.next_attempt - .last_attempt')
	[[ $d =~ ^[0-9]+$ ]] && [ "$d" -ge "$1" ] && [ "$d" -le "$2" ]
}

# Wait until the second within the 30 s wall-clock window is in [LO, HI]. early BUDGET: a
# start at second 7 or later (so past_floor never crosses a boundary) that leaves BUDGET
# seconds before the next one.
window() {
	local i s
	for ((i = 0; i < 200; i++)); do
		s=$(($(date +%s) % 30))
		[ "$s" -ge "$1" ] && [ "$s" -le "$2" ] && return 0
		sleep 0.2
	done
	die "clock window [$1, $2] never came"
}
early() { window 7 $((28 - $1)); }

# Between cases: wait until no claude-usage process runs (a detached fetch may still be
# finishing; one alive after 30 s outlived its own deadline and is recorded), end any fake
# curl left over, sweep the last case's files for leaks, then a fresh home, config folder and
# fake. The access token may appear only in the credentials and the fake's stdin and call log;
# the refresh token and the MCP token nowhere outside the credentials.
CASE=''
total_runs=0
idle() {
	wait_for '! pgrep -f "^$CU " > /dev/null' 30 || {
		echo "$CASE" >> "$T/leaks/stray"
		pkill -9 -f "^$CU "
	}
	pkill -f "^bash $T/[a-z]+/curl( |\$)" 2> /dev/null
	return 0
}
sweep() {
	[ -d "$F" ] || return 0
	total_runs=$((total_runs + $(runs)))
	[ -e "$F/refused" ] && echo "$CASE: $(tr '\n' ' ' < "$F/refused")" >> "$T/leaks/refused"
	[ -e "$F/argv-mismatch" ] && echo "$CASE: $(tr '\n' ' ' < "$F/argv-mismatch")" >> "$T/leaks/argv"
	# The test credentials (in $T/creds, or ~/.claude for the default folder) and the leak files.
	local own="^$T/(creds|leaks)/|^$HOME/\.claude/\.credentials\.json\$"
	grep -rlE 'SECRET-(REFRESH|MCP)' /tmp /dev/shm /work /small 2> /dev/null |
		grep -vE "$own" >> "$T/leaks/found"
	grep -rl 'SECRET-TOKEN' /tmp /dev/shm /work /small 2> /dev/null |
		grep -vE "$own|^$T/bin/|^$T/gw/curl\$|^$F/(stdin\.[0-9]+|calls)\$" >> "$T/leaks/found"
}
reset() {
	idle
	sweep
	rm -rf "${T:?}/home" "$T/creds" "$F" "$T/decoy-ran" "$T/xdg" "$T/xdgc" "$T/gw" "$T/elsewhere" \
		"$T"/out* "$T"/err* "$T/rcs" "$T/gets" "$T/cu2" /tmp/.config /tmp/.local /small/*
	EDIT_FAIL=''
	export CLAUDE_CONFIG_DIR=$T/creds/cfg
	unset XDG_STATE_HOME XDG_CONFIG_HOME SSLKEYLOGFILE https_proxy HTTPS_PROXY CURL_CA_BUNDLE
	mkdir -p "$HOME/.config/claude-usage" "$F"
	chmod 700 "$HOME" "$HOME/.config" "$HOME/.config/claude-usage"
	printf 'curl = %s/bin/curl\n' "$T" > "$HOME/.config/claude-usage/config"
	chmod 600 "$HOME/.config/claude-usage/config"
	EXP=$((($(date +%s) + 6 * 3600) * 1000))
	cred SECRET-TOKEN-AAA
	mode ok
}

# ---------------------------------------------------------------- cases
case_contract() {
	reset
	getj
	ok "get with no state: exit 0, exactly the empty result" '[ "$GRC" = 0 ] && jqg ". == $EMPTY"'
	ok "get creates nothing" '[ "$GRC" = 0 ] && [ ! -e "$HOME/.local" ]'
	# The reader is gone before get writes: a real EPIPE.
	(
		sleep 0.3
		exec "$CU" get 2> "$T/err"
	) | true
	rc=${PIPESTATUS[0]}
	cat "$T/err" >> "$STDERR"
	ok "get into a closed pipe: exit 0, nothing on stderr" '[ "$rc" = 0 ] && [ ! -s "$T/err" ]'
	"$CU" get 2> "$T/err" | head -c 1 > /dev/null
	rc=${PIPESTATUS[0]}
	cat "$T/err" >> "$STDERR"
	ok "get | head -c1: exit 0, nothing on stderr" '[ "$rc" = 0 ] && [ ! -s "$T/err" ]'
	"$CU" get --bogus > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "get with an unknown option: exit 2, no JSON" '[ "$rc" = 2 ] && [ ! -s "$T/out" ]'
	mkstate
	: > "$(SD)/usage.json"
	chmod 600 "$(SD)/usage.json"
	getj
	ok "empty state: the empty result" '[ "$GRC" = 0 ] && jqg ". == $EMPTY"'
	printf 'garbage{' > "$(SD)/usage.json"
	getj
	ok "garbled state: the empty result" '[ "$GRC" = 0 ] && jqg ". == $EMPTY"'
	printf '{"state_version":99,"fetched_at":1,"windows":{"five_hour":{"pct":1,"resets_at":null},"seven_day":null}}' > "$(SD)/usage.json"
	getj
	ok "future state version: the empty result" '[ "$GRC" = 0 ] && jqg ". == $EMPTY"'
	rm -f "$(SD)/usage.json"
	"$CU" get --refresh > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "get --refresh prints the same result" '[ "$rc" = 0 ] && jqf ". == $EMPTY" "$T/out"'
	wait_calls 1 10
}

case_fetch_success() {
	reset
	early 6
	CF=$CLAUDE_CONFIG_DIR/.credentials.json
	before=$(stat -c '%i %s %y %z %a' "$CF") && before+=$(sha256sum < "$CF")
	cu fetch 3< /dev/null 4>> "$T/out.fd4"
	rc=$?
	ok "fetch exits 0, one call" '[ "$rc" = 0 ] && called 1'
	ok "curl argv: exactly the fixed list" '[ -s "$F/argv.1" ] && [ ! -e "$F/argv-mismatch" ]'
	ok "token on curl's stdin as one header line" '[ "$(cat "$F/stdin.1" 2> /dev/null)" = "Authorization: Bearer SECRET-TOKEN-AAA" ]'
	ok "curl env: exactly LANG=C and PATH=/usr/bin:/bin" '[ "$(sort "$F/env.1" 2> /dev/null | tr "\n" " ")" = "LANG=C PATH=/usr/bin:/bin " ]'
	ok "curl runs in /" '[ "$(cat "$F/cwd.1" 2> /dev/null)" = / ]'
	ok "curl gets no fd but stdin, stdout, stderr (3 and 4 open in the caller)" '[ -f "$F/fds.1" ] && [ ! -s "$F/fds.1" ]'
	ok "curl runs with core dumps off" '[ "$(cat "$F/core.1" 2> /dev/null)" = 0 ]'
	ok "claude-usage's argv holds no token" '[ -s "$F/parent-cmdline.1" ] && ! grep -q SECRET "$F/parent-cmdline.1"'
	ok "claude-usage is not dumpable (its environ hidden)" '[ "$(cat "$F/parent-environ.1" 2> /dev/null)" = hidden ]'
	getj
	ok "get: exactly the contract's keys" 'jqg "keys == [\"error\",\"fetched_at\",\"schema\",\"windows\"] and (.windows | keys) == [\"five_hour\",\"seven_day\"] and [.windows[] | keys] == [[\"pct\",\"resets_at\"],[\"pct\",\"resets_at\"]]"'
	ok "numbers as sent" 'jqg ".schema == 0 and .error == null and .windows.five_hour.pct == 30.4 and .windows.seven_day.pct == 12"'
	ok "pct kept as sent (12, not 12.0)" 'printf "%s" "$G" | tr -d " \n" | grep -q "\"pct\":12[,}]"'
	ok "resets_at: whole Unix seconds" 'jqg "[.windows[].resets_at] | all(. == floor) and (.[0] - now - 7200 | fabs) < 60 and (.[1] - now - 259200 | fabs) < 60"'
	ok "fetched_at: whole Unix seconds, now" 'jqg ".fetched_at == (.fetched_at | floor) and (.fetched_at - now | fabs) < 30"'
	ok "state: exactly the design's keys" '[ "$(state "keys | join(\",\")")" = attempt_fingerprint,attempt_stamp,backoff_step,config_dir,fail_streak,fetched_at,last_attempt,last_error,next_attempt,now_times,numbers_fingerprint,numbers_stamp,pending,refusal,retry_after_seen,salt,state_version,windows ]'
	ok "state: values of the right shape" 'stateq ".state_version == 1 and (.salt | test(\"^[0-9a-f]{32}$\")) and (.attempt_fingerprint | test(\"^[0-9a-f]{64}$\")) and .numbers_fingerprint == .attempt_fingerprint and (.attempt_stamp | test(\"^[0-9]+:[0-9]+:[0-9]+:[0-9]+:[0-9]+$\")) and .numbers_stamp == .attempt_stamp and .pending == false and .last_error == null and .next_attempt == null and .backoff_step == 0 and .fail_streak == 0 and .now_times == [] and .refusal == null"'
	ok "state records the config folder" '[ "$(state .config_dir)" = "$(realpath "$CLAUDE_CONFIG_DIR")" ]'
	ok "modes: state folders 700, usage.json and .lock 600" '[ "$(stat -c %a "$HOME/.local/state" "$HOME/.local/state/claude-usage" "$(SD)" "$(SD)/usage.json" "$(SD)/.lock" 2> /dev/null | tr "\n" " ")" = "700 700 700 600 600 " ]'
	ok "the state folder holds only usage.json and .lock" '[ "$(ls -A "$(SD)" 2> /dev/null | sort | tr "\n" " ")" = ".lock usage.json " ]'
	ok "the state root holds only the key folder" '[ "$(ls -A "$HOME/.local/state/claude-usage" 2> /dev/null)" = "$(basename "$(SD)")" ]'
	after=$(stat -c '%i %s %y %z %a' "$CF") && after+=$(sha256sum < "$CF")
	ok "credentials untouched, nothing else in the config folder" 'called 1 && [ "$after" = "$before" ] && [ "$(ls -A "$CLAUDE_CONFIG_DIR")" = .credentials.json ]'
	past_floor
	cu fetch
	cu fetch
	ok "same window: no second call" 'called 1'

	reset
	early 4
	mode idle
	cu fetch
	getj
	ok "an idle window (resets_at null) and an unreported one (null)" 'called 1 && jqg ".error == null and .fetched_at != null and .windows.five_hour == {\"pct\":0,\"resets_at\":null} and .windows.seven_day == null"'
}

case_rate_limited() {
	reset
	early 8
	cu fetch
	mode r429
	age 40
	cu fetch
	rc=$?
	getj
	ok "429: exit 0, one call, rate_limited, numbers kept" '[ "$rc" = 0 ] && called 2 && jqg ".error == \"rate_limited\" and .windows.five_hour.pct == 30.4 and .fetched_at != null"'
	ok "429 without Retry-After: 60 s, retry_after_seen false" 'gap 60 62 && [ "$(state .retry_after_seen)" = false ]'
	age 40
	cu fetch
	ok "429: no call during the backoff" 'called 2'
	good=1
	for want in 120 240 480 600 600; do
		age 700
		cu fetch
		gap "$want" $((want + 2)) || good=0
	done
	ok "429: 120, 240, 480, then capped at 600 s" 'called 7 && [ "$good" = 1 ]'
	mode ok
	age 700
	cu fetch
	ok "a success ends the backoff" 'called 8 && stateq ".backoff_step == 0 and .next_attempt == null and .last_error == null"'

	# Each Retry-After on a fresh backoff (a success before it): seconds, an HTTP date, over 1 h,
	# smaller than the step, garbage, negative, empty. Aged past the longest wait (1 h) first.
	n=8
	for ra in 120 date 7200 30 soon -5 ''; do
		mode ok
		age 3700
		cu fetch
		mode r429
		[ "$ra" = date ] && ra=$(date -u -d '+300 sec' '+%a, %d %b %Y %H:%M:%S GMT')
		printf '%s' "$ra" > "$F/retry_after"
		age 40
		cu fetch
		n=$((n + 2))
		case $ra in
		120) ok "Retry-After 120: waits 120 s, retry_after_seen" 'called $n && gap 120 122 && [ "$(state .retry_after_seen)" = true ]' ;;
		*GMT) ok "Retry-After as an HTTP date: used" 'called $n && gap 295 302' ;;
		7200) ok "Retry-After over 1 h: capped at 1 h" 'called $n && gap 3590 3602' ;;
		30) ok "Retry-After smaller than the step: the step" 'called $n && gap 60 62' ;;
		soon) ok "Retry-After unparseable: as absent" 'called $n && gap 60 62' ;;
		-5) ok "Retry-After negative: as absent" 'called $n && gap 60 62' ;;
		'') ok "Retry-After empty: as absent" 'called $n && gap 60 62' ;;
		esac
	done
}

case_auth_errors() {
	reset
	early 6
	mode r401
	cu fetch
	rc=$?
	getj
	ok "401: exit 0, unauthorized" '[ "$rc" = 0 ] && called 1 && jqg ".error == \"unauthorized\""'
	ok "401: held for an hour" 'gap 3600 3602'
	age 40
	cu fetch
	ok "401: no retry with the same credentials" 'called 1 && [ "$(state .last_error)" = unauthorized ]'
	cu fetch --now
	rc=$?
	ok "401: --now refused under the hold (exit 3)" '[ "$rc" = 3 ] && called 1'
	age 3600
	cu fetch
	ok "401: retried once the hour has passed" 'called 2'
	past_floor
	mode ok
	cred SECRET-TOKEN-BBB
	cu fetch
	ok "401: new credentials end the hold at once" 'called 3 && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'

	reset
	early 4
	mode r403
	cu fetch
	getj
	ok "403: forbidden, held for an hour" 'called 1 && jqg ".error == \"forbidden\"" && gap 3600 3602'
	age 40
	cu fetch
	ok "403: no retry with the same credentials" 'called 1'
	past_floor
	mode ok
	cred SECRET-TOKEN-BBB
	cu fetch
	ok "403: new credentials end the hold at once" 'called 2 && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'

	reset
	early 6
	cred SECRET-TOKEN-AAA $((($(date +%s) - 3600) * 1000))
	cu fetch
	rc=$?
	getj
	ok "expired token: exit 0, no call, token_expired" '[ "$rc" = 0 ] && called 0 && jqg ".error == \"token_expired\""'
	ok "expired token: checked again after an hour (as 401)" 'gap 3600 3602'
	age_any 40
	cu fetch
	getj
	ok "expired token: still no call at the next boundary" 'called 0 && jqg ".error == \"token_expired\""'
	cred SECRET-TOKEN-BBB
	past_floor
	cu fetch
	ok "expired token: the refreshed one is used at once" 'called 1 && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'

	reset
	cred SECRET-TOKEN-AAA $((($(date +%s) - 30) * 1000))
	cu fetch
	ok "expired by less than 60 s: the call is made" 'called 1'
	reset
	cred SECRET-TOKEN-AAA none
	cu fetch
	ok "no expiresAt: the call is made" 'called 1'
}

case_http_errors() {
	reset
	early 8
	mode r500
	cu fetch
	rc=$?
	getj
	ok "500: exit 0, http_error" '[ "$rc" = 0 ] && called 1 && jqg ".error == \"http_error\""'
	age 40
	cu fetch
	ok "500: retried at the next boundary" 'called 2'
	age 40
	cu fetch
	ok "500 three in a row: then a 60 s backoff" 'called 3 && gap 60 62'
	age 40
	cu fetch
	ok "500: no call during that backoff" 'called 3'
	age 30
	cu fetch
	good=1
	gap 120 122 || good=0
	for want in 240 300 300; do
		age 700
		cu fetch
		gap "$want" $((want + 2)) || good=0
	done
	ok "5xx backoff: 120, 240, then capped at 300 s" 'called 7 && [ "$good" = 1 ]'

	reset
	early 6
	mode r500
	cu fetch
	age 40
	mode partial
	cu fetch
	age 40
	mode r500
	cu fetch
	ok "5xx and network errors share one streak: 60 s after three" 'called 3 && gap 60 62'
	mode ok
	age 700
	cu fetch
	mode r500
	age 40
	cu fetch
	ok "a success resets the streak: the next 5xx waits for the boundary only" 'called 5 && stateq ".fail_streak == 1 and .next_attempt == null"'

	reset
	mode r404
	cu fetch
	ok "404: http_error, backoff 60 s" 'called 1 && [ "$(state .last_error)" = http_error ] && gap 60 62'
	good=1
	for want in 120 240 480 960 1920 3600 3600; do
		age 3700
		cu fetch
		gap "$want" $((want + 2)) || good=0
	done
	ok "other 4xx: doubling to a 1 h cap" 'called 8 && [ "$good" = 1 ]'
	past_floor
	mode ok
	cred SECRET-TOKEN-BBB
	cu fetch
	ok "other 4xx: new credentials end the backoff at once" 'called 9 && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'

	reset
	mode html
	cu fetch
	getj
	ok "HTML with 200: bad_response, backoff 60 s" 'called 1 && jqg ".error == \"bad_response\"" && gap 60 62'
	reset
	mode bigjson
	cu fetch
	getj
	ok "valid JSON over 64 KiB: bad_response (the cap, not the parser)" 'called 1 && jqg ".error == \"bad_response\""'
	reset
	mode fitjson
	cu fetch
	getj
	ok "valid JSON just under 64 KiB: its numbers" 'called 1 && jqg ".error == null and .windows.five_hour.pct == 30.4"'
	reset
	mode oldcurl
	cu fetch
	rc=$?
	getj
	ok "curl older than 7.84 (tail unexpanded): exit 0, no_curl" '[ "$rc" = 0 ] && called 1 && jqg ".error == \"no_curl\""'
	reset
	mode partial
	cu fetch
	getj
	ok "curl exit 28 with part of a body and a tail: network" 'called 1 && jqg ".error == \"network\""'
	reset
	mode x7
	cu fetch
	getj
	ok "curl exit 7, nothing printed: network" '[ "$(runs)" = 1 ] && jqg ".error == \"network\""'
	reset
	mode x2
	cu fetch
	getj
	ok "curl exit 2 (failed to start): no_curl" '[ "$(runs)" = 1 ] && jqg ".error == \"no_curl\""'
	good=1
	for x in x1 x4; do
		reset
		mode "$x"
		cu fetch
		getj
		[ "$(runs)" = 1 ] && jqg ".error == \"no_curl\"" || good=0
	done
	ok "curl exit 1 and 4: no_curl" '[ "$good" = 1 ]'
	reset
	chmod 644 "$T/bin/curl"
	cu fetch
	rc=$?
	chmod 755 "$T/bin/curl"
	getj
	ok "a configured curl without the exec bit: exit 0, no_curl" '[ "$rc" = 0 ] && [ "$(runs)" = 0 ] && jqg ".error == \"no_curl\""'

	# The last numbers stay through every kind of failure.
	reset
	early 4
	cu fetch
	good=1
	for m in r401 r403 r500 partial html oldcurl x7; do
		mode ok
		age 3700
		cu fetch
		mode "$m"
		age 3700
		cu fetch
		getj
		jqg ".error != null and .windows.five_hour.pct == 30.4 and .fetched_at != null" || good=0
	done
	ok "401, 403, 5xx, network, bad_response, no_curl: the last numbers kept" '[ "$good" = 1 ] && called 15'
	reset
	mode hang
	s=$(date +%s)
	timeout 40 "$CU" fetch 2>> "$STDERR"
	e=$(($(date +%s) - s))
	getj
	wait_for '! pgrep -f "^bash $T/bin/curl" > /dev/null' 2
	gone=$?
	ok "a hung curl: the watchdog ends it, network" 'called 1 && [ "$e" -le 15 ] && jqg ".error == \"network\""'
	ok "the watchdog ends curl itself, not only the wait" 'called 1 && [ "$gone" = 0 ]'
	reset
	mode trickle
	s=$(date +%s)
	timeout 40 "$CU" fetch 2>> "$STDERR"
	e=$(($(date +%s) - s))
	getj
	ok "a body that trickles out: watchdog, network" 'called 1 && [ "$e" -le 15 ] && jqg ".error == \"network\""'
	reset
	mode nostdin
	s=$(date +%s)
	timeout 40 "$CU" fetch 2>> "$STDERR"
	e=$(($(date +%s) - s))
	getj
	ok "a curl that never reads stdin: watchdog, network" '[ "$(runs)" = 1 ] && [ "$e" -le 15 ] && jqg ".error == \"network\""'
}

case_credentials() {
	reset
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	rc=$?
	getj
	ok "no credentials file: exit 0, no call, no_credentials" '[ "$rc" = 0 ] && called 0 && jqg "$HIDDEN and .error == \"no_credentials\""'
	reset
	printf '{"mcpOAuth":{"srv|1":{"accessToken":"SECRET-MCP-ONE"}}}' > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "no claudeAiOauth (API-key setup): no_credentials" 'called 0 && jqg ".error == \"no_credentials\""'
	reset
	cred 'SECRET-TOKEN-AAA\r\nX-Injected: 1'
	cu fetch
	rc=$?
	getj
	ok "token with CR/LF: exit 0, no call, bad_credentials" '[ "$rc" = 0 ] && called 0 && jqg ".error == \"bad_credentials\""'
	reset
	cred "SECRET-TOKEN-$(head -c 5000 /dev/zero | tr '\0' A)"
	cu fetch
	getj
	ok "token over 4096 bytes: no call, bad_credentials" 'called 0 && jqg ".error == \"bad_credentials\""'
	reset
	printf '{"claudeAiOauth":{"expiresAt":%s}}' "$EXP" > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "claudeAiOauth without accessToken: bad_credentials" 'called 0 && jqg ".error == \"bad_credentials\""'
	reset
	{
		creds_json SECRET-TOKEN-AAA | sed 's/}$/,"pad":"/'
		head -c 1100000 /dev/zero | tr '\0' A
		printf '"}'
	} > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "credentials over 1 MiB: bad_credentials" 'called 0 && jqg ".error == \"bad_credentials\""'
	reset
	pad_creds 1048576 > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	ok "credentials of exactly 1 MiB: used" 'called 1 && [ "$(stat -c %s "$CLAUDE_CONFIG_DIR/.credentials.json")" = 1048576 ]'
	reset
	pad_creds 1048577 > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "credentials of 1 MiB and one byte: bad_credentials" '[ "$(runs)" = 0 ] && jqg ".error == \"bad_credentials\"" && [ "$(stat -c %s "$CLAUDE_CONFIG_DIR/.credentials.json")" = 1048577 ]'
	reset
	early 6
	printf '{"claudeAiOauth":' > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	getj
	ok "unparsable credentials: bad_credentials" 'called 0 && jqg ".error == \"bad_credentials\""'
	cred SECRET-TOKEN-AAA
	past_floor
	cu fetch
	ok "after bad_credentials: good ones are used at once, in the same window" 'called 1'
	reset
	printf '{"claudeAiOauth":{"accessToken":"SECRET-TOKEN-AAA","expiresAt":"SECRET-REFRESH-X"}}' > "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu_e "$T/err" fetch
	getj
	ok "a type error in the credentials: bad_credentials, nothing echoed" 'called 0 && jqg ".error == \"bad_credentials\"" && ! grep -q SECRET "$T/err"'
	reset
	early 6
	chmod 000 "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	rc=$?
	getj
	past_floor
	m=$(spawned "$CU" get --refresh)
	ok "credentials that cannot be opened: bad_credentials, then no spawn on every read" '[ "$rc" = 0 ] && called 0 && jqg ".error == \"bad_credentials\"" && [ "$m" = 0 ]'
	chmod 600 "$CLAUDE_CONFIG_DIR/.credentials.json"

	reset
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	mkfifo "$CLAUDE_CONFIG_DIR/.credentials.json"
	s=$(date +%s)
	timeout 20 "$CU" get > "$T/out" 2>> "$STDERR"
	rc=$?
	e=$(($(date +%s) - s))
	ok "a FIFO as credentials: get only stats it (no hang, exit 0)" '[ "$rc" = 0 ] && [ "$e" -le 2 ] && jqf ".schema == 0" "$T/out"'
	s=$(date +%s)
	timeout 20 "$CU" fetch 2>> "$STDERR"
	e=$(($(date +%s) - s))
	getj
	ok "a FIFO as credentials: fetch does not hang, bad_credentials" '[ "$e" -le 5 ] && called 0 && jqg ".error == \"bad_credentials\""'

	reset
	early 6
	mkdir -p "$T/creds/dot"
	mv "$CLAUDE_CONFIG_DIR/.credentials.json" "$T/creds/dot/c.json"
	ln -s "$T/creds/dot/c.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
	cu fetch
	ok "symlinked credentials are followed" 'called 1 && [ "$(lastcall)" = SECRET-TOKEN-AAA ]'
	creds_json SECRET-TOKEN-BBB > "$T/creds/dot/n.json" && mv -f "$T/creds/dot/n.json" "$T/creds/dot/c.json"
	past_floor
	cu fetch
	ok "a switch behind the symlink is seen" 'called 2 && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'
}

case_accounts() {
	reset
	early 6
	cu fetch
	cred SECRET-TOKEN-BBB
	getj
	ok "switch: old numbers hidden at once" 'called 1 && jqg "$HIDDEN and .error == \"account_changed\""'
	past_floor
	cu fetch
	getj
	ok "switch: one call with the new token, its numbers" 'called 2 && [ "$(lastcall)" = SECRET-TOKEN-BBB ] && jqg ".error == null and .windows.five_hour.pct == 80"'

	reset
	early 6
	cu fetch
	cred SECRET-TOKEN-BBB
	past_floor
	mode r429
	cu fetch
	getj
	ok "switch then 429: old numbers never shown" 'called 2 && jqg "$HIDDEN and .error == \"account_changed\""'
	chmod 400 "$CLAUDE_CONFIG_DIR/.credentials.json"
	past_floor
	cu fetch
	getj
	ok "switch, 429, then chmod: still hidden, no call" 'called 2 && jqg "$HIDDEN and .error == \"account_changed\""'

	reset
	early 6
	cu fetch
	cred SECRET-TOKEN-BBB
	past_floor
	la0=$(state .last_attempt)
	mode hang
	"$CU" fetch 2>> "$STDERR" &
	p=$!
	wait_calls 2 5
	kill -9 "$p" 2> /dev/null
	wait "$p" 2> /dev/null
	getj
	ok "switch then SIGKILL mid-call: old numbers never shown" 'called 2 && jqg "$HIDDEN and .error == \"account_changed\""'
	ok "the killed attempt was recorded first" '[ "$(state .last_attempt)" -gt "$la0" ] && [ "$(state .pending)" = true ]'
	mode ok
	past_floor
	cu fetch
	ok "after the killed attempt: no retry in the same window" 'called 2'
	age 40
	cu fetch
	getj
	ok "next boundary: the new account's numbers" 'called 3 && [ "$(lastcall)" = SECRET-TOKEN-BBB ] && jqg ".error == null and .windows.five_hour.pct == 80"'

	reset
	early 6
	cp -p "$CLAUDE_CONFIG_DIR/.credentials.json" "$T/creds/saved-a.json"
	cu fetch
	cred SECRET-TOKEN-BBB
	past_floor
	cu fetch
	cp -p "$T/creds/saved-a.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
	getj
	ok "restore with cp -p: hidden until confirmed" 'called 2 && jqg "$HIDDEN and .error == \"account_changed\""'
	past_floor
	cu fetch
	getj
	ok "restore with cp -p: refetched with the restored token" 'called 3 && [ "$(lastcall)" = SECRET-TOKEN-AAA ] && jqg ".windows.five_hour.pct == 30.4"'

	reset
	early 8
	cp -p "$CLAUDE_CONFIG_DIR/.credentials.json" "$T/creds/saved-a.json"
	cu fetch
	cred SECRET-TOKEN-BBB
	past_floor
	mode r429
	cu fetch
	cp -p "$T/creds/saved-a.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
	past_floor
	cu fetch
	getj
	ok "switch, 429, restore: the restored account's numbers back with no call" 'called 2 && jqg ".windows.five_hour.pct == 30.4 and .error == \"rate_limited\""'

	reset
	early 6
	cu fetch
	cred_inplace SECRET-TOKEN-CCC
	past_floor
	cu fetch
	getj
	ok "in-place token refresh: one call" 'called 2 && [ "$(lastcall)" = SECRET-TOKEN-CCC ] && jqg ".windows.five_hour.pct == 55"'

	reset
	early 6
	cu fetch
	cred_inplace SECRET-TOKEN-AAA "$EXP" SECRET-MCP-TWO
	getj
	ok "MCP-only change: hidden until a fetch looks" 'jqg "$HIDDEN and .error == \"account_changed\""'
	past_floor
	cu fetch
	getj
	ok "MCP-only change: no call, numbers back" 'called 1 && jqg ".error == null and .windows.five_hour.pct == 30.4"'
	m=$(spawned "$CU" get --refresh)
	ok "MCP-only change: once looked at, get --refresh spawns nothing" '[ "$m" = 0 ] && called 1'
	chmod 400 "$CLAUDE_CONFIG_DIR/.credentials.json"
	getj
	ok "chmod-only change: hidden until a fetch looks" 'jqg "$HIDDEN and .error == \"account_changed\""'
	past_floor
	cu fetch
	getj
	ok "chmod-only change: no call, numbers back" 'called 1 && jqg ".error == null and .windows.five_hour.pct == 30.4"'
	m=$(spawned "$CU" get --refresh)
	ok "chmod-only change: once looked at, get --refresh spawns nothing" '[ "$m" = 0 ] && called 1'

	# New credentials during a 429 backoff: one detached fetch looks (no call), then no more.
	reset
	early 8
	cu fetch
	mode r429
	age 40
	cu fetch
	cred SECRET-TOKEN-BBB
	past_floor
	m1=$(spawned "$CU" get --refresh)
	wait_for '! pgrep -f "^$CU fetch" > /dev/null' 5
	m2=$(spawned "$CU" get --refresh)
	getj
	ok "switch during a 429 backoff: one fetch looks, no call, then nothing spawned" '[ "$m1" -ge 1 ] && [ "$m2" = 0 ] && called 2 && jqg "$HIDDEN and .error == \"account_changed\""'
	age 70
	cu fetch
	ok "switch during a 429 backoff: the new token is called once the backoff ends" 'called 3 && [ "$(lastcall)" = SECRET-TOKEN-BBB ]'

	reset
	early 6
	cu fetch
	cred SECRET-TOKEN-BBB
	cu fetch
	cred SECRET-TOKEN-CCC
	cu fetch
	getj
	ok "credentials churn: the floor holds" 'called 1 && jqg "$HIDDEN and .error == \"account_changed\""'
	past_floor
	cu fetch
	ok "credentials churn: one call with the latest" 'called 2 && [ "$(lastcall)" = SECRET-TOKEN-CCC ]'

	reset
	early 4
	cu fetch
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	getj
	ok "logout: numbers hidden" 'called 1 && jqg "$HIDDEN and .error == \"no_credentials\""'

	reset
	early 6
	creds_json SECRET-TOKEN-BBB > "$T/creds/next.json"
	printf '#!/bin/sh\nmv -f "%s" "%s"\nrm -f "%s"\n' "$T/creds/next.json" "$CLAUDE_CONFIG_DIR/.credentials.json" "$F/during" > "$F/during"
	chmod 755 "$F/during"
	cu fetch
	getj
	ok "switch during a fetch: its numbers are hidden" 'called 1 && jqg "$HIDDEN and .error == \"account_changed\""'
	past_floor
	cu fetch
	getj
	ok "switch during a fetch: refetched with the new token" 'called 2 && [ "$(lastcall)" = SECRET-TOKEN-BBB ] && jqg ".windows.five_hour.pct == 80"'
}

case_config() {
	reset
	CLAUDE_CONFIG_DIR=creds/cfg "$CU" get > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "relative CLAUDE_CONFIG_DIR: get says bad_config, exit 0" '[ "$rc" = 0 ] && jqf "$HIDDEN and .error == \"bad_config\"" "$T/out"'
	# Relative to $T it would name the real test credentials: it must still not be used.
	(cd "$T" && CLAUDE_CONFIG_DIR=creds/cfg exec "$CU" fetch 2>> "$STDERR")
	rc=$?
	ok "relative CLAUDE_CONFIG_DIR: fetch exits 1, no call" '[ "$rc" = 1 ] && called 0'
	m=$(cd "$T" && spawned env CLAUDE_CONFIG_DIR=creds/cfg "$CU" get --refresh)
	r0=$(runs)
	m2=$(spawned "$CU" get --refresh)
	wait_calls 1 5
	ok "relative CLAUDE_CONFIG_DIR: get --refresh spawns nothing (an absolute one does)" '[ "$m" = 0 ] && [ "$r0" = 0 ] && [ "$m2" -ge 1 ] && called 1'

	reset
	mkdir -p "$HOME/.claude" && mv "$CLAUDE_CONFIG_DIR/.credentials.json" "$HOME/.claude/"
	CLAUDE_CONFIG_DIR='' "$CU" fetch 2>> "$STDERR"
	ok "empty CLAUDE_CONFIG_DIR means ~/.claude" 'called 1 && [ -f "$(CLAUDE_CONFIG_DIR=$HOME/.claude SD)/usage.json" ]'

	reset
	export XDG_STATE_HOME=$T/xdg
	cu fetch
	ok "XDG_STATE_HOME is ignored" 'called 1 && [ -f "$(SD)/usage.json" ] && [ ! -e "$T/xdg" ]'
	unset XDG_STATE_HOME
	ln -s "$CLAUDE_CONFIG_DIR" "$T/creds/link"
	same=1
	for d in "$T/creds/link" "$T/creds/./cfg/" "$T/creds/../creds/cfg"; do
		CLAUDE_CONFIG_DIR=$d "$CU" get 2>> "$STDERR" | jq -e ".windows.five_hour.pct == 30.4" > /dev/null || same=0
	done
	ok "three spellings of one folder share one state" 'called 1 && [ "$same" = 1 ]'
	CLAUDE_CONFIG_DIR=$T/creds/missing "$CU" get > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "a missing config folder: no_credentials, exit 0" '[ "$rc" = 0 ] && jqf ".error == \"no_credentials\"" "$T/out"'

	reset
	early 6
	chmod 620 "$HOME/.config/claude-usage/config"
	cu fetch
	rc=$?
	getj
	ok "a group-writable config file is ignored: exit 0, no_curl" '[ "$rc" = 0 ] && [ "$(runs)" = 0 ] && jqg ".error == \"no_curl\""'
	chmod 600 "$HOME/.config/claude-usage/config"
	past_floor
	cu fetch
	ok "no_curl: not checked again within the same window" 'called 0'
	age_any 40
	cu fetch
	ok "no_curl is checked again at the next boundary" 'called 1'

	reset
	{
		printf 'curl = %s/bin/curl\n' "$T"
		head -c 70000 /dev/zero | tr '\0' '#'
	} > "$HOME/.config/claude-usage/config"
	cu fetch
	getj
	ok "a config file over 64 KiB is ignored: no_curl" '[ "$(runs)" = 0 ] && jqg ".error == \"no_curl\""'

	# /tmp is root's and writable by all: without the owner check, the state would go there
	# and the call would be made.
	reset
	mkdir -p /tmp/.config/claude-usage && chmod 700 /tmp/.config /tmp/.config/claude-usage
	printf 'curl = %s/bin/curl\n' "$T" > /tmp/.config/claude-usage/config
	chmod 600 /tmp/.config/claude-usage/config
	HOME=/tmp "$CU" fetch > "$T/out" 2>> "$STDERR"
	rc=$?
	ok "a HOME the user does not own: fetch refused, exit 1, no call, nothing written" '[ "$rc" = 1 ] && [ "$(runs)" = 0 ] && [ ! -e /tmp/.local ]'
	m=$(spawned env HOME=/tmp "$CU" get --refresh)
	ok "a HOME the user does not own: get --refresh spawns nothing" '[ "$m" = 0 ] && [ "$(runs)" = 0 ] && [ "$rc" = 1 ]'
	rm -rf /tmp/.config /tmp/.local
}

case_state_folder() {
	reset
	mkdir -p "$HOME/.local/state/claude-usage" "$T/elsewhere"
	chmod 700 "$HOME/.local" "$HOME/.local/state" "$HOME/.local/state/claude-usage"
	ln -s "$T/elsewhere" "$(SD)"
	cu fetch
	rc=$?
	getj
	ok "a symlinked state folder: refused, exit 1, nothing written there" '[ "$rc" = 1 ] && [ "$(runs)" = 0 ] && [ -z "$(ls -A "$T/elsewhere")" ] && jqg "$HIDDEN and .error == \"state_unusable\""'
	mv "$CLAUDE_CONFIG_DIR/.credentials.json" "$T/creds/away.json"
	getj
	mv "$T/creds/away.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
	ok "state_unusable comes before no_credentials" 'jqg "$HIDDEN and .error == \"state_unusable\""'
	rm -f "$(SD).refused"
	m=$(spawned "$CU" get --refresh)
	r0=$(runs)
	[ -e "$(SD).refused" ] && r0=refusal-recorded
	rm -f "$(SD)"
	m2=$(spawned "$CU" get --refresh)
	wait_calls 1 5
	ok "an unusable state: get --refresh spawns nothing (a usable one does)" '[ "$m" = 0 ] && [ "$r0" = 0 ] && [ "$m2" -ge 1 ] && called 1'

	reset
	mkdir -p "$T/elsewhere"
	mkstate
	ln -s "$T/elsewhere/usage.json" "$(SD)/usage.json"
	cu fetch
	rc=$?
	getj
	ok "usage.json a symlink: refused, exit 1, nothing written there" '[ "$rc" = 1 ] && [ "$(runs)" = 0 ] && [ -z "$(ls -A "$T/elsewhere")" ] && jqg "$HIDDEN and .error == \"state_unusable\""'

	reset
	mkdir -p "$HOME/.local/state" "$T/elsewhere"
	chmod 700 "$HOME/.local" "$HOME/.local/state"
	ln -s "$T/elsewhere" "$HOME/.local/state/claude-usage"
	cu fetch
	rc=$?
	getj
	ok "claude-usage/ a symlink: refused, exit 1, nothing written there" '[ "$rc" = 1 ] && [ "$(runs)" = 0 ] && [ -z "$(ls -A "$T/elsewhere")" ] && jqg "$HIDDEN and .error == \"state_unusable\""'

	# Full: the state on a 64 KiB tmpfs, filled up after a first good fetch.
	reset
	early 6
	home=$HOME
	export HOME=/small/home
	mkdir -p "$HOME/.config/claude-usage" && chmod 700 "$HOME" "$HOME/.config" "$HOME/.config/claude-usage"
	printf 'curl = %s/bin/curl\n' "$T" > "$HOME/.config/claude-usage/config"
	chmod 600 "$HOME/.config/claude-usage/config"
	cu fetch
	age 40
	head -c 1M /dev/zero > /small/fill 2> /dev/null
	cu fetch
	rc=$?
	m=$(spawned "$CU" get --refresh)
	ok "a full state folder: exit 1, no call" '[ "$rc" = 1 ] && called 1'
	ok "a full state folder: get --refresh spawns nothing more in that window" '[ "$m" = 0 ]'
	rm -rf /small/fill /small/home
	export HOME=$home

	# The binary removed while it runs (an uninstall): run a deleted copy through an open fd.
	reset
	cp "$CU" "$T/cu2" && chmod 755 "$T/cu2"
	m=$(
		exec 3< "$T/cu2"
		rm -f "$T/cu2"
		spawned /proc/self/fd/3 get --refresh
	)
	cp "$CU" "$T/cu2" && chmod 755 "$T/cu2"
	(
		exec 3< "$T/cu2"
		rm -f "$T/cu2"
		exec /proc/self/fd/3 fetch 2>> "$STDERR"
	)
	ns=absent
	[ -e "$HOME/.local/state/claude-usage" ] && ns=present
	m2=$(spawned "$CU" get --refresh)
	wait_calls 1 5
	wait_for '[ "$(state .pending)" = false ]' 10
	ok "binary removed: get --refresh spawns nothing (present: it does)" '[ "$m" = 0 ] && [ "$m2" -ge 1 ] && called 1'
	ok "binary removed: fetch makes no state folder (present: it does)" '[ "$ns" = absent ] && [ -e "$(SD)/usage.json" ]'

	reset
	early 4
	mode r429
	cu fetch
	state_edit '.next_attempt = .last_attempt + 7200'
	age 3601
	cu fetch
	ok "a next_attempt over 1 h after the last attempt is capped at 1 h" 'called 2'

	reset
	mkstate
	mkdir "$(SD)/usage.json"
	cu_e "$T/err1" fetch
	rc1=$?
	cu_e "$T/err2" fetch
	rc2=$?
	getj
	ok "usage.json a folder: exit 1, no call, state_unusable" '[ "$rc1" = 1 ] && [ "$(runs)" = 0 ] && jqg "$HIDDEN and .error == \"state_unusable\""'
	ok "a refusal: one <4> line, none on a repeat" '[ "$(wc -l < "$T/err1")" = 1 ] && grep -q "^<4>" "$T/err1" && [ "$rc2" = 1 ] && [ ! -s "$T/err2" ]'
	ok "a refusal is recorded beside the folder" '[ -f "$(SD).refused" ]'
	echo write_failed > "$(SD).refused"
	cu_e "$T/err4" fetch
	ok "a refusal with another cause than the recorded one: said again" '[ "$(wc -l < "$T/err4")" = 1 ] && grep -q "^<4>" "$T/err4"'
	rm -f "$(SD).refused"
	"$CU" fetch 2> /dev/null
	cu_e "$T/err3" fetch
	ok "a refusal nobody heard (stderr /dev/null) is said by the next fetch" '[ "$(wc -l < "$T/err3")" = 1 ] && grep -q "^<4>" "$T/err3"'

	reset
	mkstate
	mkdir "$(SD)/.lock"
	cu fetch
	rc=$?
	getj
	m=$(spawned "$CU" get --refresh)
	ok ".lock not a file: fetch exit 1, no call; get state_unusable, spawns nothing" '[ "$rc" = 1 ] && [ "$(runs)" = 0 ] && jqg "$HIDDEN and .error == \"state_unusable\"" && [ "$m" = 0 ]'

	reset
	mkdir -p "$T/elsewhere"
	mkstate
	ln -s "$T/elsewhere/x" "$(SD)/.lock"
	cu fetch
	rc=$?
	getj
	ok ".lock a symlink: fetch exit 1, no call, nothing made through it; get state_unusable" '[ "$rc" = 1 ] && [ "$(runs)" = 0 ] && [ ! -e "$T/elsewhere/x" ] && jqg "$HIDDEN and .error == \"state_unusable\""'

	reset
	early 6
	cu fetch
	chmod 755 "$(SD)" "$HOME/.local/state/claude-usage"
	getj
	ok "get: folders with a wrong mode are usable and left as they are" 'jqg ".error == null and .windows.five_hour.pct == 30.4" && [ "$(stat -c %a "$(SD)" "$HOME/.local/state/claude-usage" | tr "\n" " ")" = "755 755 " ]'
	age 40
	cu fetch
	ok "fetch sets claude-usage/ and the key folder back to 700" 'called 2 && [ "$(stat -c %a "$(SD)" "$HOME/.local/state/claude-usage" | tr "\n" " ")" = "700 700 " ]'

	reset
	early 6
	cu fetch
	state_edit '.pad = ("a" * 70000)'
	getj
	ok "usage.json over 64 KiB: as no state" 'jqg ". == $EMPTY"'

	reset
	mkstate
	chmod 500 "$(SD)"
	cu fetch
	rc=$?
	ok "a state folder of the user's with a wrong mode: set to 700, the call made" '[ "$rc" = 0 ] && called 1 && [ "$(stat -c %a "$(SD)")" = 700 ]'

	reset
	early 6
	cu fetch
	: 2> /dev/null > "$(SD)/.usage.json.AbC123" && touch -d '-2 min' "$(SD)/.usage.json.AbC123"
	: 2> /dev/null > "$(SD)/.usage.json.XyZ789"
	age 40
	cu fetch
	ok "a temp file over 60 s old is removed, a newer one kept" 'called 2 && [ ! -e "$(SD)/.usage.json.AbC123" ] && [ -e "$(SD)/.usage.json.XyZ789" ]'

	reset
	mkstate
	now=$(date +%s)
	printf '{"state_version":99,"last_attempt":%s,"next_attempt":%s,"backoff_step":3,"fail_streak":0,"last_error":"rate_limited","retry_after_seen":false,"now_times":[]}' \
		$((now - 100)) $((now + 500)) > "$(SD)/usage.json"
	chmod 600 "$(SD)/usage.json"
	cu fetch
	rc=$?
	ok "a future state version keeps its backoff: no call" '[ "$rc" = 0 ] && [ "$(runs)" = 0 ]'
	state_edit '.next_attempt = (now | floor) - 1'
	cu fetch
	ok "a future state version: a call once that backoff ends" 'called 1'

	reset
	early 4
	cu fetch
	state_edit '.last_attempt = (now | floor) + 3600'
	cu fetch
	ok "a last_attempt over 60 s in the future counts as due" 'called 2'
}

case_curl_choice() {
	reset
	rm -f "$HOME/.config/claude-usage/config"
	# Each on its own: curl runs in /, so ./curl found first would hide the absolute folder.
	(cd / && PATH=$T/decoy:$PATH exec "$CU" fetch 2>> "$STDERR")
	getj
	first=$(jq -r .error <<< "$G"):$(runs):$([ -e "$T/decoy-ran" ] && echo ran)
	reset
	rm -f "$HOME/.config/claude-usage/config"
	(cd "$T/decoy" && PATH=.:$PATH exec "$CU" fetch 2>> "$STDERR")
	getj
	ok "curl is never taken from PATH (., a project folder)" '[ ! -e "$T/decoy-ran" ] && [ "$(runs)" = 0 ] && [ "$first" = no_curl:0: ] && jqg ".error == \"no_curl\""'

	reset
	mkdir -p "$T/xdgc/claude-usage"
	printf 'curl = %s/decoy/curl\n' "$T" > "$T/xdgc/claude-usage/config"
	chmod 600 "$T/xdgc/claude-usage/config"
	export XDG_CONFIG_HOME=$T/xdgc
	cu fetch
	ok "XDG_CONFIG_HOME is ignored" '[ ! -e "$T/decoy-ran" ] && called 1'
	unset XDG_CONFIG_HOME

	reset
	export SSLKEYLOGFILE=$T/keys https_proxy=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 CURL_CA_BUNDLE=$T/ca
	cu fetch
	ok "SSLKEYLOGFILE, proxies, CA bundle: not passed to curl" 'called 1 && [ "$(sort "$F/env.1" | tr "\n" " ")" = "LANG=C PATH=/usr/bin:/bin " ]'
	unset SSLKEYLOGFILE https_proxy HTTPS_PROXY CURL_CA_BUNDLE

	reset
	mkdir -p "$T/gw" && cp "$T/bin/curl" "$T/gw/curl" && chmod 775 "$T/gw"
	printf 'curl = %s/gw/curl\n' "$T" > "$HOME/.config/claude-usage/config"
	cu fetch
	getj
	ok "a configured curl in a group-writable folder: refused, no_curl" '[ "$(runs)" = 0 ] && jqg ".error == \"no_curl\""'

	reset
	printf 'curl = bin/curl\n' > "$HOME/.config/claude-usage/config"
	(cd "$T" && exec "$CU" fetch 2>> "$STDERR")
	getj
	ok "a relative configured curl: refused, no_curl" '[ "$(runs)" = 0 ] && jqg ".error == \"no_curl\""'
}

case_refresh() {
	reset
	early 8
	s=$(date +%s%N)
	"$CU" get --refresh > "$T/out" 2>> "$STDERR"
	rc=$?
	e=$((($(date +%s%N) - s) / 1000000))
	ok "get --refresh returns at once with the result" '[ "$rc" = 0 ] && [ "$e" -lt 1000 ] && jqf ". == $EMPTY" "$T/out"'
	wait_calls 1 10
	wait_for '[ "$(state .pending)" = false ]' 10
	ok "get --refresh starts one fetch" 'called 1 && [ "$(state .fetched_at)" != null ]'
	ok "the detached fetch: argv exactly its own path and fetch" '[ "$(cat "$F/parent-cmdline.1" 2> /dev/null)" = "$CU fetch " ]'
	ok "the detached fetch runs in its own session" '[ -s "$F/parent-sid.1" ] && [ "$(cat "$F/parent-sid.1")" != "$(ps -o sid= -p $$ | tr -d " ")" ]'
	past_floor
	m=$(spawned "$CU" get --refresh)
	ok "not due: nothing spawned" '[ "$m" = 0 ] && called 1'

	reset
	mode slow
	s=$(date +%s%N)
	"$CU" get --refresh 2>&1 3>&1 | cat > "$T/out.pipe"
	e=$((($(date +%s%N) - s) / 1000000))
	ok "the reader's pipes (stdout, stderr, fd 3) reach EOF at once" '[ "$e" -lt 1500 ] && jqf ".schema == 0" "$T/out.pipe"'
	wait_calls 1 10
	m=$(spawned "$CU" get --refresh)
	m2=$(spawned "$CU" get --refresh)
	ok "none started while one runs" '[ "$m" = 0 ] && [ "$m2" = 0 ] && called 1'

	# A fetch that runs no curl (no credentials): the pid count is exactly the one fetch.
	reset
	rm -f "$CLAUDE_CONFIG_DIR/.credentials.json"
	m=$(spawned "$CU" get --refresh)
	wait_for '[ -f "$(SD)/usage.json" ]' 5
	ok "get --refresh: exactly one process, no thread" '[ "$m" = 1 ] && [ "$(state .last_error)" = no_credentials ]'

	reset
	mode slow
	setsid bash -c '"$0" get --refresh > /dev/null 2>> "$1"; sleep 30' "$CU" "$STDERR" &
	p=$!
	wait_calls 1 5
	kill -TERM -- "-$p" 2> /dev/null
	wait "$p" 2> /dev/null
	wait_for '[ "$(state .pending)" = false ]' 10
	getj
	ok "the fetch survives a kill of the reader's process group" 'called 1 && jqg ".windows.five_hour.pct == 30.4"'

	reset
	mkdir -p "$T/creds/other" "$T/creds/x"
	creds_json SECRET-TOKEN-CCC > "$T/creds/other/.credentials.json"
	chmod 600 "$T/creds/other/.credentials.json"
	CLAUDE_CONFIG_DIR=$T/creds/x/../other "$CU" get --refresh > /dev/null 2>> "$STDERR"
	wait_calls 1 10
	ok "the detached fetch fetches for the reader's config folder" 'called 1 && [ "$(lastcall)" = SECRET-TOKEN-CCC ]'

	reset
	early 14
	cu fetch
	flock -o -x "$(SD)/.lock" 2> /dev/null sleep 12 &
	lp=$!
	sleep 0.3
	state_edit '.last_attempt -= 100'
	getj
	ok "a lock held, the last attempt over 60 s old: stalled, numbers kept" 'jqg ".error == \"stalled\" and .windows.five_hour.pct == 30.4"'
	m=$(spawned "$CU" get --refresh)
	cu fetch
	rc=$?
	ok "while the lock is held: get --refresh spawns nothing, fetch exits 0 without a call" '[ "$m" = 0 ] && [ "$rc" = 0 ] && called 1'
	unlock "$lp"
	"$CU" get --refresh > /dev/null 2>> "$STDERR"
	wait_calls 2 5
	ok "once the lock is free: get --refresh starts the fetch" 'called 2'
}

case_now() {
	reset
	window 12 16
	cu fetch
	age 11
	cu fetch
	ok "plain fetch: not due before the boundary" 'called 1'
	cu fetch --now
	rc=$?
	ok "--now: fetches before the boundary (exit 0)" '[ "$rc" = 0 ] && called 2 && [ "$(state ".now_times | length")" = 1 ]'
	age 11
	cu fetch --now
	rc=$?
	ok "--now: one per :00/:30 window (exit 3)" '[ "$rc" = 3 ] && called 2'
	state_edit '.now_times = [] | .last_attempt = (now | floor) - 7'
	cu fetch --now
	rc=$?
	ok "--now within 10 s of the last attempt: exit 3" '[ "$rc" = 3 ] && called 2'
	state_edit '.last_attempt = (now | floor) - 11'
	cu fetch --now
	rc=$?
	ok "--now: allowed again once both pass" '[ "$rc" = 0 ] && called 3'

	reset
	window 12 16
	cu fetch
	age 40
	cu fetch --now
	rc=$?
	ok "--now when due anyway: a normal fetch, no --now slot used" '[ "$rc" = 0 ] && called 2 && [ "$(state ".now_times | length")" = 0 ]'
	age 11
	cu fetch --now
	rc=$?
	ok "--now: the slot is still free" '[ "$rc" = 0 ] && called 3'

	reset
	window 12 16
	cu fetch
	state_edit '(now | floor) as $n | .last_attempt = $n - 11 | .now_times = [$n - 500, $n - 400, $n - 300, $n - 200, $n - 100, $n - 50]'
	cu fetch --now
	rc=$?
	ok "--now: six per 10 minutes (exit 3)" '[ "$rc" = 3 ] && called 1'
	state_edit '.now_times[0] -= 200'
	cu fetch --now
	rc=$?
	ok "--now: allowed once one of the six is over 10 minutes old" '[ "$rc" = 0 ] && called 2'

	reset
	window 12 16
	mode r429
	cu fetch
	age 11
	cu fetch --now
	rc=$?
	ok "--now under a 429 backoff: exit 3" '[ "$rc" = 3 ] && called 1'

	reset
	window 12 14
	cu fetch
	age 11
	flock -o -x "$(SD)/.lock" 2> /dev/null sleep 3 &
	lp=$!
	sleep 0.3
	s=$(date +%s)
	cu fetch --now
	rc=$?
	e=$(($(date +%s) - s))
	unlock "$lp"
	ok "--now waits for a running fetch, then fetches" '[ "$rc" = 0 ] && [ "$e" -ge 2 ] && called 2'

	reset
	window 12 14
	cu fetch
	age 11
	flock -o -x "$(SD)/.lock" 2> /dev/null sleep 30 &
	lp=$!
	sleep 0.3
	s=$(date +%s)
	cu fetch --now
	rc=$?
	e=$(($(date +%s) - s))
	unlock "$lp"
	ok "--now still busy after 15 s: exit 3" '[ "$rc" = 3 ] && [ "$e" -ge 14 ] && [ "$e" -le 18 ] && called 1'

	reset
	window 12 16
	cu fetch
	age 11
	for _ in $(seq 20); do
		(
			"$CU" fetch --now 2>> "$STDERR"
			echo $? >> "$T/rcs"
		) &
	done
	wait
	ok "20 concurrent --now: one call, one exit 0, nineteen exit 3" 'called 2 && [ "$(grep -cx 0 "$T/rcs")" = 1 ] && [ "$(grep -cx 3 "$T/rcs")" = 19 ]'
}

case_robustness() {
	reset
	early 8
	cu fetch
	mode r429
	age 40
	cu fetch
	age 700
	la0=$(state .last_attempt)
	mode hang
	"$CU" fetch 2>> "$STDERR" &
	p=$!
	wait_calls 3 5
	kill -9 "$p" 2> /dev/null
	wait "$p" 2> /dev/null
	ok "SIGKILL mid-call: the attempt was recorded first" 'called 3 && [ "$(state .last_attempt)" -gt "$la0" ] && [ "$(state .pending)" = true ]'
	getj
	ok "SIGKILL mid-call: stalled (over rate_limited), numbers kept" 'jqg ".error == \"stalled\" and .windows.five_hour.pct == 30.4 and .fetched_at != null"'
	mode ok
	past_floor
	cu fetch
	ok "SIGKILL mid-call: the next fetch in that window does not call" 'called 3'
	age 40
	cu fetch
	getj
	ok "stalled clears with the next attempt" 'called 4 && jqg ".error == null and .windows.five_hour.pct == 30.4"'

	reset
	early 8
	mode slow
	for _ in $(seq 10); do
		(
			"$CU" fetch 2>> "$STDERR"
			echo $? >> "$T/rcs"
		) &
	done
	wait
	ok "10 concurrent fetches at one boundary: one call, all exit 0" 'called 1 && [ "$(grep -cx 0 "$T/rcs")" = 10 ]'

	"$CU" status > "$T/out.status" 2>> "$STDERR"
	rc=$?
	ok "status runs (exit 0, some output)" '[ "$rc" = 0 ] && [ -s "$T/out.status" ]'
	ok "status warns: this uid has no passwd entry (the timer's home may differ)" 'grep -q "^warning: .*no passwd entry" "$T/out.status"'

	# The 25 s deadline: a refusal line written to a full pipe nobody reads blocks for ever,
	# and the parent ignores SIGALRM (inherited across exec). The fetch resets it and dies at
	# 25 s by SIGALRM (142), well before timeout's 40 s (124). 4096-byte writes are atomic, so
	# dd stops with the pipe exactly full and the line cannot fit.
	mkfifo "$T/full"
	exec 3<> "$T/full"
	timeout 3 dd if=/dev/zero of="$T/full" bs=4096 2> /dev/null
	s=$(date +%s)
	# In a subshell whose own stderr is dropped: bash's "Alarm clock" notice goes there.
	(
		CLAUDE_CONFIG_DIR=relative timeout 40 bash -c 'trap "" ALRM; exec "$0" fetch' "$CU" 2>&3
		echo $? > "$T/rc.deadline"
	) 2> /dev/null
	rc=$(cat "$T/rc.deadline")
	e=$(($(date +%s) - s))
	exec 3>&-
	rm -f "$T/full"
	ok "the 25 s deadline: blocked on a full stderr, SIGALRM ignored by the parent, killed at 25 s" '[ "$rc" = 142 ] && [ "$e" -ge 24 ] && [ "$e" -le 27 ]'
}

# install, install --status, uninstall: the files, the manifest, the refusals. This container
# has no systemd: the timer is enabled by its link alone (the systemctl paths are the sandbox's).
I='' UD='' MAN='' VER=''
sha() { sha256sum < "$1" | cut -d' ' -f1; }
want_manifest() { # TIMER
	printf 'claude-usage manifest 1\nversion %s\ntimer %s\nfile bin %s -\nfile service %s -\nfile timer %s -\n' \
		"$VER" "$1" "$(sha "$CU")" "$(sha /src/systemd/claude-usage.service)" "$(sha /src/systemd/claude-usage.timer)"
}
installed() { # TIMER: every file as installed, the manifest exact and finished
	cmp -s "$CU" "$I" && cmp -s /src/systemd/claude-usage.service "$UD/claude-usage.service" &&
		cmp -s /src/systemd/claude-usage.timer "$UD/claude-usage.timer" &&
		[ "$(stat -c %a "$I" "$UD/claude-usage.service" "$UD/claude-usage.timer" "$(dirname "$MAN")" "$MAN" | tr '\n' ' ')" = "700 600 600 700 600 " ] &&
		[ "$(cat "$MAN")" = "$(want_manifest "$1")" ]
}
linked() { [ "$(readlink "$UD/timers.target.wants/claude-usage.timer")" = "$UD/claude-usage.timer" ]; }
# run CMD…: its stdout in $T/out, stderr in $T/err (and the leak sweep's file), rc.
rc=''
run() {
	"$@" > "$T/out" 2> "$T/err"
	rc=$?
	cat "$T/err" >> "$STDERR"
}
says() { grep -qF -- "$1" "$T/out" "$T/err"; }
fresh() { rm -rf "$HOME/.local" "$HOME/.config/systemd" "$T/elsewhere"; }
cfg_list() { find "$CLAUDE_CONFIG_DIR" -printf '%p %s %T@\n' | sort; }

case_install() {
	reset
	I=$HOME/.local/bin/claude-usage UD=$HOME/.config/systemd/user MAN=$HOME/.local/share/claude-usage/manifest
	VER=$("$CU" --version | cut -d' ' -f2)
	cfg0=$(cfg_list)
	run "$CU" install --status
	ok "install --status before any install: not installed, exit 1" '[ "$rc" = 1 ] && says "not installed"'
	run "$CU" uninstall
	ok "uninstall before any install: nothing to do, exit 0, nothing made" '[ "$rc" = 0 ] && says "not installed: nothing to do" && [ ! -e "$HOME/.local" ]'

	run "$CU" install
	ok "install: exit 0, the program, the units and the manifest (timer on), modes 700/600" '[ "$rc" = 0 ] && installed on'
	ok "install without systemd: the timer enabled by its link, and said that nothing runs it" 'linked && says "nothing runs it"'
	ok "install says the Claude-Statusline note" 'says "two calls per 30 s"'
	ok "install wrote nothing in the Claude config folder" '[ "$(cfg_list)" = "$cfg0" ]'
	ino=$(stat -c %i "$I" "$UD/claude-usage.service" "$UD/claude-usage.timer" "$MAN" | tr '\n' ' ')
	run "$CU" install
	ok "install again: exit 0, files up to date (not rewritten), the manifest the same" \
		'[ "$rc" = 0 ] && says "files: up to date" && installed on && [ "$(stat -c %i "$I" "$UD/claude-usage.service" "$UD/claude-usage.timer" | tr "\n" " ")" = "${ino% * } " ]'
	run "$I" get
	ok "the installed program answers get" '[ "$rc" = 0 ] && jq -e .schema "$T/out" > /dev/null'
	run "$CU" install --status
	ok "install --status: healthy, exit 0; whether the timer runs cannot be asked here" '[ "$rc" = 0 ] && says healthy && says "cannot be asked"'

	run "$CU" install --no-timer
	ok "--no-timer: recorded, the link removed" '[ "$rc" = 0 ] && installed off && [ ! -e "$UD/timers.target.wants/claude-usage.timer" ]'
	run "$CU" install
	ok "install again without a flag keeps the timer off" '[ "$rc" = 0 ] && installed off && [ ! -L "$UD/timers.target.wants/claude-usage.timer" ]'
	run "$CU" install --status
	ok "install --status with the timer off: healthy" '[ "$rc" = 0 ] && says healthy'
	ln -s "$UD/claude-usage.timer" "$UD/timers.target.wants/claude-usage.timer"
	run "$CU" install --status
	ok "install --status: the timer off but its link there, a problem (exit 4)" '[ "$rc" = 4 ] && says "problem: the timer is off"'
	run "$CU" install --timer
	ok "--timer: recorded, linked" '[ "$rc" = 0 ] && installed on && linked'
	rm "$UD/timers.target.wants/claude-usage.timer"
	run "$CU" install --status
	ok "install --status: the timer wanted but not linked, a problem (exit 4)" '[ "$rc" = 4 ] && says "wanted but not enabled"'
	run "$CU" install
	ok "install again links it" '[ "$rc" = 0 ] && linked'
	run "$CU" install --timer --status
	ok "install --status with another option: usage error, exit 2" '[ "$rc" = 2 ]'

	echo "# mine" >> "$UD/claude-usage.timer"
	t0=$(sha "$UD/claude-usage.timer") m0=$(sha "$MAN")
	run "$CU" install --status
	ok "install --status: a unit changed since the install, a problem naming it (exit 4)" '[ "$rc" = 4 ] && says "problem: ~/.config/systemd/user/claude-usage.timer was changed"'
	run "$CU" install
	ok "install: a changed unit is refused (exit 1), kept, the drop-in named, the manifest as it was" \
		'[ "$rc" = 1 ] && says "claude-usage.timer.d/" && [ "$(sha "$UD/claude-usage.timer")" = "$t0" ] && [ "$(sha "$MAN")" = "$m0" ]'
	cp /src/systemd/claude-usage.timer "$UD/claude-usage.timer"
	printf 'old program\n' > "$I"
	run "$CU" install
	ok "install: a changed program is refused too" '[ "$rc" = 1 ] && says "claude-usage was changed since the install"'
	cp "$CU" "$I"
	touch "$UD/.claude-usage.service.cu-abcdef" "$HOME/.local/bin/.claude-usage.cu-zz9zz9" "$UD/.other.cu-abcdef"
	run "$CU" install
	ok "install removes temp files a killed run left (its own names only)" \
		'[ "$rc" = 0 ] && [ ! -e "$UD/.claude-usage.service.cu-abcdef" ] && [ ! -e "$HOME/.local/bin/.claude-usage.cu-zz9zz9" ] && [ -e "$UD/.other.cu-abcdef" ]'
	rm -f "$UD/.other.cu-abcdef"

	# The lock: one install or uninstall at a time.
	flock -o "$(dirname "$MAN")" sleep 30 &
	lk=$!
	wait_for 'flock -n -o "$(dirname "$MAN")" true; [ $? = 1 ]' 5
	run "$CU" install
	r1=$rc
	run "$CU" uninstall
	r2=$rc
	run "$CU" install --status
	r3=$rc
	unlock "$lk"
	ok "while another run holds the lock: install and uninstall refused (exit 1), status says so (exit 4)" \
		'[ "$r1" = 1 ] && [ "$r2" = 1 ] && [ "$r3" = 4 ] && says "an install or uninstall is running" && installed on'

	# What is not claude-usage's is never written over or removed.
	fresh
	mkdir -p "$HOME/.local/bin" && printf 'another program\n' > "$I"
	run "$CU" install
	ok "install: another program at its path, refused (exit 1), nothing written" \
		'[ "$rc" = 1 ] && says "is there and not claude-usage" && [ "$(cat "$I")" = "another program" ] && [ ! -e "$MAN" ] && [ ! -e "$UD/claude-usage.service" ]'
	[ ! -e "$HOME/.local/share" ] && [ ! -e "$HOME/.config/systemd" ] && none1=1 || none1=0
	cp "$CU" "$I"
	run "$CU" install
	ok "install: the same program already there (no manifest, mode 755) is taken as its own, mode 700" '[ "$rc" = 0 ] && installed on'
	fresh
	mkdir -p "$UD" "$T/elsewhere" && echo keep > "$T/elsewhere/unit" && ln -s "$T/elsewhere/unit" "$UD/claude-usage.service"
	run "$CU" install
	ok "install: a symlink at a unit path, refused, its target untouched, nothing written" \
		'[ "$rc" = 1 ] && says "symbolic link" && [ "$(cat "$T/elsewhere/unit")" = keep ] && [ ! -e "$I" ] && [ ! -e "$MAN" ]'
	ok "a refused install leaves no folder behind (none made before the checks)" '[ "$none1" = 1 ] && [ ! -e "$HOME/.local" ]'
	fresh
	mkdir -p "$UD/timers.target.wants" "$T/elsewhere" && cp /src/systemd/claude-usage.timer "$T/elsewhere/"
	ln -s "$T/elsewhere/claude-usage.timer" "$UD/timers.target.wants/claude-usage.timer"
	run "$CU" install
	ok "install: a link to a claude-usage.timer in another folder at the timer's link, refused" '[ "$rc" = 1 ] && says "not the timer'"'"'s link" && [ ! -e "$MAN" ]'
	fresh
	mkdir -p "$T/elsewhere/config" && chmod 700 "$T/elsewhere" "$T/elsewhere/config"
	mv "$HOME/.config/claude-usage" "$T/elsewhere/config/" && rmdir "$HOME/.config" && ln -s "$T/elsewhere/config" "$HOME/.config"
	run "$CU" install
	ok "install: a symlinked ~/.config (dotfiles) is followed" '[ "$rc" = 0 ] && installed on && [ -f "$T/elsewhere/config/systemd/user/claude-usage.timer" ] && linked'
	run "$CU" install --status
	ok "install --status through it: healthy" '[ "$rc" = 0 ]'
	run "$CU" uninstall
	ok "uninstall through it: the units and the link removed" '[ "$rc" = 0 ] && [ ! -e "$T/elsewhere/config/systemd/user/claude-usage.timer" ] && [ ! -L "$UD/timers.target.wants/claude-usage.timer" ]'
	rm "$HOME/.config" && mv "$T/elsewhere/config" "$HOME/.config"
	fresh
	mkdir -p "$HOME/.local/bin" && chmod 757 "$HOME/.local/bin"
	run "$CU" install
	ok "install: a ~/.local/bin others can write, refused, nothing written" '[ "$rc" = 1 ] && says "only you can write" && [ ! -e "$MAN" ] && [ ! -e "$I" ]'
	chmod 775 "$HOME/.local/bin"
	run "$CU" install
	ok "install: group-writable for your own group only (private groups), accepted" '[ "$rc" = 0 ] && installed on'
	fresh
	mkdir -p "$HOME/.local/share" "$T/elsewhere/share" && ln -s "$T/elsewhere/share" "$HOME/.local/share/claude-usage"
	run "$CU" install
	ok "install: the manifest's folder a symlink, refused, nothing written" '[ "$rc" = 1 ] && [ -z "$(ls -A "$T/elsewhere/share")" ] && [ ! -e "$I" ]'
	fresh
	mkdir -p "$HOME/.local/share/claude-usage" && printf 'claude-usage manifest 1\nfile bin /etc/passwd\n' > "$MAN"
	run "$CU" install
	r1=$rc
	run "$CU" uninstall
	ok "a manifest it did not write: install and uninstall refuse (exit 1), it stays" \
		'[ "$r1" = 1 ] && [ "$rc" = 1 ] && says "not one claude-usage wrote" && [ -e "$MAN" ] && [ ! -e "$I" ]'
	run env HOME=/ "$CU" install
	ok "install with a HOME not yours: refused (exit 1)" '[ "$rc" = 1 ] && says "HOME is not a folder of yours"'

	# A run that stopped part-way is finished by the next.
	fresh
	mkdir -p "$HOME/.local/bin" "$HOME/.local/share/claude-usage" && printf 'old program\n' > "$I"
	printf 'claude-usage manifest 1\nversion 0.0.0\ntimer off\npending install\nfile bin %s %s\nfile service %s -\nfile timer %s -\n' \
		"$(sha "$CU")" "$(sha "$I")" "$(sha /src/systemd/claude-usage.service)" "$(sha /src/systemd/claude-usage.timer)" > "$MAN"
	run "$CU" install
	ok "an install stopped part-way (the old program still there, no units): the next finishes it, keeping its timer choice" \
		'[ "$rc" = 0 ] && installed off'
	# Kills at points 0 to 4 ms in (0.1 ms apart, round and round), until enough of them landed
	# mid-run (or 400 tries). The delay is bash's own read -t on a FIFO nobody writes: a forked
	# sleep takes as long as a whole install (about 3 ms here).
	mkfifo "$T/never"
	exec 5<> "$T/never"
	hits=0 good=1 n=0
	while [ "$n" -lt 41 ] || { [ "$hits" -lt 3 ] && [ "$n" -lt 400 ]; }; do
		d=$(printf '0.%04d' $((n % 41)))
		n=$((n + 1))
		fresh
		"$CU" install > /dev/null 2>> "$STDERR" &
		p=$!
		read -rt "$d" -u 5 _
		kill -9 "$p" 2> /dev/null
		wait "$p" 2> /dev/null
		grep -q '^pending install' "$MAN" 2> /dev/null && hits=$((hits + 1))
		run "$CU" install
		{ [ "$rc" = 0 ] && installed on && [ -z "$(find "$HOME/.local" "$UD" -name '.*.cu-*')" ]; } || good=0
	done
	echo "  info  SIGKILL mid-install: $n tries, $hits landed mid-run"
	ok "SIGKILL mid-install (at least 3 landed mid-run): the next install finishes it, no temp file left" '[ "$good" = 1 ] && [ "$hits" -ge 3 ]'
	# An upgrade that stops after its new program is written (the unit folder read-only), then
	# the earlier release installed again (a rollback by hand): the pending manifest, written
	# first, counts the new program as claude-usage's. The new program: this one with a byte
	# appended (an ELF file runs with trailing data).
	cp "$CU" "$T/cu2" && printf x >> "$T/cu2" && chmod 755 "$T/cu2"
	fresh
	run "$CU" install
	rm "$UD/claude-usage.service" && chmod 500 "$UD"
	run "$T/cu2" install
	r1=$rc
	chmod 700 "$UD"
	ok "an upgrade that cannot write a unit: stopped part-way (exit 3), the new program in place, the manifest pending" \
		'[ "$r1" = 3 ] && says "stopped part-way" && cmp -s "$T/cu2" "$I" && grep -qx "pending install" "$MAN"'
	run "$CU" install
	ok "then the earlier release installs over it" '[ "$rc" = 0 ] && installed on'
	# The same with kills, until some leave the new program with the manifest pending.
	hits=0 good=1 n=0
	while [ "$n" -lt 41 ] || { [ "$hits" -lt 2 ] && [ "$n" -lt 400 ]; }; do
		d=$(printf '0.%04d' $((n % 41)))
		n=$((n + 1))
		fresh
		run "$CU" install
		"$T/cu2" install > /dev/null 2>> "$STDERR" &
		p=$!
		read -rt "$d" -u 5 _
		kill -9 "$p" 2> /dev/null
		wait "$p" 2> /dev/null
		cmp -s "$T/cu2" "$I" && grep -q '^pending install' "$MAN" && hits=$((hits + 1))
		run "$CU" install
		{ [ "$rc" = 0 ] && installed on; } || good=0
	done
	echo "  info  an upgrade killed part-way: $n tries, $hits with the new program in and the manifest pending"
	ok "an upgrade killed part-way (at least 2 with the new program in, the manifest pending): the earlier release installs over it" \
		'[ "$good" = 1 ] && [ "$hits" -ge 2 ]'
	exec 5>&-
	rm -f "$T/never"

	# uninstall
	cu fetch
	run "$CU" uninstall
	ok "uninstall: the program, the units, the link, the manifest and its folder removed; the state kept" \
		'[ "$rc" = 0 ] && [ ! -e "$I" ] && [ ! -e "$UD/claude-usage.service" ] && [ ! -e "$UD/claude-usage.timer" ] && [ ! -L "$UD/timers.target.wants/claude-usage.timer" ] && [ ! -e "$HOME/.local/share/claude-usage" ] && [ -f "$(SD)/usage.json" ]'
	ok "uninstall wrote nothing in the Claude config folder" '[ "$(cfg_list)" = "$cfg0" ]'
	run "$CU" install
	echo "# mine" >> "$UD/claude-usage.service"
	mkdir "$UD/claude-usage.service.d" && echo '[Service]' > "$UD/claude-usage.service.d/x.conf"
	run "$CU" uninstall
	ok "uninstall keeps a changed unit and your drop-in, and says so" \
		'[ "$rc" = 0 ] && says "kept ~/.config/systemd/user/claude-usage.service: changed" && says "kept ~/.config/systemd/user/claude-usage.service.d/" && [ -f "$UD/claude-usage.service.d/x.conf" ] && [ ! -e "$I" ]'
	rm -rf "$UD/claude-usage.service" "$UD/claude-usage.service.d"
	run "$CU" install
	mkdir -p "$HOME/.local/share/claude-usage"
	sed -i 's/^timer on$/timer on\npending uninstall/' "$MAN"
	run "$CU" install
	ok "install while an uninstall is unfinished: refused (exit 1)" '[ "$rc" = 1 ] && says "uninstall is unfinished"'
	run "$CU" uninstall
	ok "uninstall again finishes it" '[ "$rc" = 0 ] && [ ! -e "$MAN" ] && [ ! -e "$I" ]'

	# uninstall and running fetches: it waits for their locks, 15 s at most.
	run "$CU" install
	flock -o "$(SD)/.lock" sleep 3 &
	lk=$!
	wait_for 'flock -n -o "$(SD)/.lock" true; [ $? = 1 ]' 5
	s=$(date +%s)
	run "$CU" uninstall
	e=$(($(date +%s) - s))
	unlock "$lk"
	ok "uninstall waits for a fetch holding a state lock, then finishes" '[ "$rc" = 0 ] && [ "$e" -ge 2 ] && [ ! -e "$MAN" ]'
	run "$CU" install
	flock -o "$(SD)/.lock" sleep 40 &
	lk=$!
	wait_for 'flock -n -o "$(SD)/.lock" true; [ $? = 1 ]' 5
	s=$(date +%s)
	run "$CU" uninstall
	e=$(($(date +%s) - s))
	ok "a lock held past 15 s: uninstall stops (exit 3), the program gone, the units and the pending manifest kept" \
		'[ "$rc" = 3 ] && [ "$e" -ge 14 ] && [ "$e" -le 20 ] && [ ! -e "$I" ] && [ -e "$UD/claude-usage.timer" ] && grep -qx "pending uninstall" "$MAN" && says "release"'
	unlock "$lk"
	run "$CU" uninstall
	ok "uninstall again (from the release's copy) finishes it" '[ "$rc" = 0 ] && [ ! -e "$MAN" ] && [ ! -e "$UD/claude-usage.timer" ]'

	# --purge
	run "$CU" install
	touch "$HOME/.local/state/claude-usage/notes.txt"
	run "$CU" uninstall --purge
	ok "uninstall --purge with a file of another kind in the state folder: refused (exit 1), nothing removed" \
		'[ "$rc" = 1 ] && says "notes.txt is not claude-usage" && [ -e "$MAN" ] && [ -e "$I" ] && [ -f "$(SD)/usage.json" ]'
	rm "$HOME/.local/state/claude-usage/notes.txt"
	echo state_unusable > "$HOME/.local/state/claude-usage/$(basename "$(SD)").refused"
	touch "$(SD)/.usage.json.AbC123"
	run "$CU" uninstall --purge
	ok "uninstall --purge: everything removed, the state folder too (its notes and temp files)" \
		'[ "$rc" = 0 ] && [ ! -e "$HOME/.local/state/claude-usage" ] && [ ! -e "$MAN" ] && [ ! -e "$I" ] && [ -z "$(find "$HOME/.local" -type f)" ]'
	ok "uninstall --purge left the credentials and the config folder as they were" '[ "$(cfg_list)" = "$cfg0" ]'
	cu fetch
	run "$CU" uninstall --purge
	ok "uninstall --purge with nothing installed: the state removed" '[ "$rc" = 0 ] && [ ! -e "$HOME/.local/state/claude-usage" ]'

	# install.sh, from the release folder, run from elsewhere.
	fresh
	rm -rf "$T/rel"
	mkdir -p "$T/rel" && cp "$CU" /src/install.sh "$T/rel/" && (cd "$T/rel" && sha256sum claude-usage install.sh > SHA256SUMS)
	(cd / && "$T/rel/install.sh" --no-timer) > "$T/out" 2> "$T/err"
	rc=$?
	ok "install.sh run from another folder: checks the sums, installs, passes --no-timer on" '[ "$rc" = 0 ] && installed off'
	fresh
	printf 'x' >> "$T/rel/claude-usage"
	(cd / && "$T/rel/install.sh") > "$T/out" 2> "$T/err"
	rc=$?
	ok "install.sh with a file that does not match SHA256SUMS: refused, nothing installed" '[ "$rc" = 1 ] && says "do not match" && [ ! -e "$HOME/.local" ]'
}

# ---------------------------------------------------------------- run
all=(contract fetch_success rate_limited auth_errors http_errors credentials accounts config
	state_folder curl_choice refresh now robustness install)
sel=("${all[@]}")
if [ -n "${CU_ONLY:-}" ]; then
	IFS=, read -r -a sel <<< "$CU_ONLY"
	for c in "${sel[@]}"; do [[ " ${all[*]} " == *" $c "* ]] || die "unknown case: $c"; done
fi
for c in "${sel[@]}"; do
	CASE=$c
	echo "== $c"
	"case_$c"
done
CASE=end
idle
sweep
grep -l SECRET "$STDERR" >> "$T/leaks/found"
echo "== leaks and leftovers"
ok "no token anywhere but the credentials and the fake's log" '[ "$total_runs" -gt 0 ] && [ ! -s "$T/leaks/found" ]'
[ -s "$T/leaks/found" ] && sed 's/^/        leak: /' "$T/leaks/found" | sort -u
ok "every curl run had the fixed argument list" '[ "$total_runs" -gt 0 ] && [ ! -s "$T/leaks/argv" ]'
ok "curl never got a malformed or non-test token" '[ "$total_runs" -gt 0 ] && [ ! -s "$T/leaks/refused" ]'
ok "no claude-usage process outlived its case" '[ "$total_runs" -gt 0 ] && [ ! -s "$T/leaks/stray" ]'
echo "cli: $pass passed, $fail failed"
[ "$fail" = 0 ]
