#!/usr/bin/env bash
# claude-usage in a throwaway container booting real systemd: the real curl against an HTTPS
# stand-in for api.anthropic.com, the systemd user units (timer, hardening, journal), and the
# checks that need root (running as root, files of another user, a read-only state folder).
# Leaves nothing behind.
#   sandbox/run.sh            build, run every check, tear down  (about 2 min)
#   KEEP=1 sandbox/run.sh     same, but leave the container and image up to look around
#   RELEASE=FILE sandbox/run.sh   with a downloaded release tarball instead of one built here
#                             (the release check: run it in a checkout of that release's tag)
#   sandbox/run.sh --down     remove what a finished or killed run left (refused while one runs)
# Needs Docker, and internet for the build only (Ubuntu packages, BuildKit); the test container
# has none.
#
# Safety, as Agent-Session-Control's tests/sandbox.sh. The container has no network but
# loopback (the stand-in listens there; --add-host points api.anthropic.com at it), no
# published ports, no bind mounts, is not privileged, gains no new privileges, has fewer
# capabilities (systemd needs SYS_ADMIN), a 1 GiB memory cap with no swap, a process limit, a
# small /tmp, and the highest OOM score. Its whole HostConfig is compared with the one written
# below before it starts, and checks from inside follow the start; either aborts the run before
# any test. Root inside is still root on the host with fewer capabilities: this box is for the
# repo's own code, which runs as an ordinary user (sbuser).
# The image holds the repo's tracked files (uncommitted edits are tested once git-added) and the
# release binary scripts/cargo builds from the working tree. It is built by a throwaway buildx
# builder (memory-capped; its cache goes with it); base image and BuildKit pinned by digest.
# Only objects with the exact names below are ever removed (the BuildKit image only when this
# run pulled it).
# The test token SECRET-TOKEN-SBX is a fixture (the stand-in's mode file accepts it). It goes
# into the container on stdin, never on a host command line.
# No pipefail on purpose: checks pipe into grep -q, whose early exit would fail the writer.
# Each check is a single-quoted condition that chk evals later, so no expansion is meant there,
# and variables such as UID1 or want are read only inside those conditions.
# shellcheck disable=SC2016,SC2034
set -u
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE=cu-sandbox:latest NET=cu-sandbox-net C=cu-sandbox BUILDER=cu-sandbox-builder
# Bump: docker buildx imagetools inspect moby/buildkit:<version>, its top-level Digest.
BK=moby/buildkit:v0.30.0@sha256:0168606be2315b7c807a03b3d8aa79beefdb31c98740cebdffdfeebf31190c9f
UID1=2001 UID2=2002 RUN1=/run/user/2001 H1=/home/sbuser
CU=$H1/.local/bin/claude-usage
OPTS=(--hostname cu-sandbox --network none
	--add-host api.anthropic.com:127.0.0.1 --sysctl net.ipv4.ip_unprivileged_port_start=0
	--cap-add SYS_ADMIN --cap-drop NET_RAW --cap-drop NET_BIND_SERVICE --cap-drop MKNOD
	--security-opt apparmor=unconfined --security-opt no-new-privileges --cgroupns=private
	--tmpfs /run --tmpfs /run/lock --tmpfs '/tmp:size=256m,exec' --memory 1g --memory-swap 1g --pids-limit 1024
	--oom-score-adj 1000 --log-opt max-size=10m -e container=docker --stop-signal SIGRTMIN+3)
INIT=(sh -c 'mount -o remount,rw /sys/fs/cgroup 2>/dev/null; exec /sbin/init')
# The HostConfig docker 29.5 gives a container made with OPTS, every field written out here and
# never taken from a container (a wrong one would match itself). Any other value, a field
# missing or one more, fails the check. Dns only: null (docker 29.5) is read as [] (the docker
# of GitHub's runners); both mean no DNS server set. MaskedPaths is held apart: docker adds a
# path per CPU of the host, so every path below is required and more of that kind only are
# allowed.
WANT='{
  "AutoRemove": false, "Binds": null,
  "BlkioDeviceReadBps": [], "BlkioDeviceReadIOps": [], "BlkioDeviceWriteBps": [], "BlkioDeviceWriteIOps": [],
  "BlkioWeight": 0, "BlkioWeightDevice": [],
  "CapAdd": ["CAP_SYS_ADMIN"], "CapDrop": ["CAP_MKNOD", "CAP_NET_BIND_SERVICE", "CAP_NET_RAW"],
  "Cgroup": "", "CgroupParent": "", "CgroupnsMode": "private", "ConsoleSize": [0, 0], "ContainerIDFile": "",
  "CpuCount": 0, "CpuPercent": 0, "CpuPeriod": 0, "CpuQuota": 0, "CpuRealtimePeriod": 0, "CpuRealtimeRuntime": 0,
  "CpuShares": 0, "CpusetCpus": "", "CpusetMems": "",
  "DeviceCgroupRules": null, "DeviceRequests": null, "Devices": [],
  "Dns": [], "DnsOptions": [], "DnsSearch": [], "ExtraHosts": ["api.anthropic.com:127.0.0.1"], "GroupAdd": null,
  "IOMaximumBandwidth": 0, "IOMaximumIOps": 0, "IpcMode": "private", "Isolation": "", "Links": null,
  "LogConfig": {"Config": {"max-size": "10m"}, "Type": "json-file"},
  "Memory": 1073741824, "MemoryReservation": 0, "MemorySwap": 1073741824, "MemorySwappiness": null,
  "NanoCpus": 0, "NetworkMode": "none", "OomKillDisable": false, "OomScoreAdj": 1000,
  "PidMode": "", "PidsLimit": 1024, "PortBindings": {}, "Privileged": false, "PublishAllPorts": false,
  "ReadonlyPaths": ["/proc/bus", "/proc/fs", "/proc/irq", "/proc/sys", "/proc/sysrq-trigger"],
  "ReadonlyRootfs": false, "RestartPolicy": {"MaximumRetryCount": 0, "Name": "no"}, "Runtime": "runc",
  "SecurityOpt": ["apparmor=unconfined", "no-new-privileges"], "ShmSize": 67108864,
  "Sysctls": {"net.ipv4.ip_unprivileged_port_start": "0"},
  "Tmpfs": {"/run": "", "/run/lock": "", "/tmp": "size=256m,exec"},
  "UTSMode": "", "Ulimits": [], "UsernsMode": "", "VolumeDriver": "", "VolumesFrom": null
}'
MASKED='["/proc/acpi", "/proc/asound", "/proc/interrupts", "/proc/kcore", "/proc/keys", "/proc/latency_stats",
  "/proc/sched_debug", "/proc/scsi", "/proc/timer_list", "/proc/timer_stats", "/sys/devices/virtual/powercap",
  "/sys/firmware"]'

pass=0 fail=0 FAILED=() LOCKED=0
ok() { echo "  ok    $1"; pass=$((pass + 1)); }
bad() {
	echo "  FAIL  $1"; fail=$((fail + 1)); FAILED+=("$1")
	[ -n "${2-}" ] || return 0
	printf '%s\n' "$2" | head -40 | sed 's/^/        /'
}
# chk NAME TEST [SHOW]: both are code; SHOW runs only on failure, after the test.
chk() { if eval "$2"; then ok "$1"; else bad "$1" "$([ -z "${3:-}" ] || eval "$3" 2>&1)"; fi; }
info() { echo "  info  $*"; }
die() { echo "ABORT: $*" >&2; exit 2; }
wait_for() { local n=$1; shift; for _ in $(seq "$n"); do eval "$*" && return 0; sleep 1; done; return 1; }
x() { docker exec "$C" "$@"; }
# As sbuser, with its user manager's environment (systemctl --user works).
ux() { docker exec -u sbuser -e XDG_RUNTIME_DIR=$RUN1 -e DBUS_SESSION_BUS_ADDRESS=unix:path=$RUN1/bus -w $H1 "$C" "$@"; }
started() { awk '{print $22}' "/proc/$1/stat" 2>/dev/null; }

# What differs between the container's HostConfig and WANT, a line each, or "same". Only a
# finished check prints "same", so a check that dies half-way fails.
hostconfig_diff() {
	local got out rc
	got=$(docker inspect "$C" --format '[{{json .HostConfig}}, {{json .Mounts}}, {{json .NetworkSettings.Networks}}]' 2>&1) ||
		{ echo "docker inspect: $got"; return 1; }
	out=$(jq -rn --argjson want "$WANT" --argjson masked "$MASKED" --argjson hm "$got" '
		def show($o; $k): if $o | has($k) then $o[$k] | tojson else "(none)" end;
		($hm[0] | if type == "object" and has("Dns") and .Dns == null then .Dns = [] else . end) as $got
		| $hm[1] as $mounts | $hm[2] as $nets | $got.MaskedPaths as $m
		| (([$got, $want] | map(keys) | add | unique - ["MaskedPaths"])[] as $k
		   | select(($got | has($k)) != ($want | has($k)) or $got[$k] != $want[$k])
		   | "\($k): \(show($got; $k)), expected \(show($want; $k))"),
		  (if ($m | type) != "array" then "MaskedPaths: \($m | tojson), expected a list"
		   else (($masked - $m)[] | "MaskedPaths: \(.) is not hidden"),
		        (($m - $masked)[] | select(type != "string" or contains("\n")
		                                   or (test("^/sys/devices/system/cpu/cpu[0-9]+/thermal_throttle$") | not))
		         | "MaskedPaths: \(tojson) is more than expected")
		   end),
		  (if $mounts != [] then "Mounts: \($mounts | tojson), expected none" else empty end),
		  (if ($nets | type) != "object" or ($nets | keys) != ["none"] then "Networks: \($nets | tojson), expected none only" else empty end)' 2>&1)
	rc=$?
	if [ "$rc" != 0 ]; then echo "jq (exit $rc): $out"; return 1; fi
	[ -n "$out" ] && { printf '%s\n' "$out"; return 1; }
	echo same
}
# What is wrong inside the started container, or "same": PID 1's capability bounding set
# (docker's defaults, SYS_ADMIN added, NET_RAW, NET_BIND_SERVICE, MKNOD dropped), its OOM score,
# /proc/sys and /sys read-only, loopback only, the host's Docker bridge out of reach, no new
# privileges.
inside_diff() {
	local v gw probs=()
	v=$(x sed -n 's/^CapBnd:\t//p' /proc/1/status 2>&1); [ "$v" = 00000000a02401fb ] || probs+=("CapBnd of PID 1: $v")
	v=$(x cat /proc/1/oom_score_adj 2>&1); [ "$v" = 1000 ] || probs+=("oom_score_adj of PID 1: $v")
	v=$(x awk '$5 == "/sys" || $5 == "/proc/sys" {print $5, substr($6, 1, 3)}' /proc/self/mountinfo 2>&1 | sort)
	[ "$v" = "$(printf '/proc/sys ro,\n/sys ro,')" ] || probs+=("/proc/sys and /sys: ${v//$'\n'/; }")
	v=$(x ls /sys/class/net 2>&1); [ "$v" = lo ] || probs+=("network devices: ${v//$'\n'/ }")
	gw=$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}' 2>/dev/null)
	if [ -z "$gw" ]; then probs+=("the host's Docker bridge address: not found")
	elif x timeout 3 bash -c "echo > /dev/tcp/$gw/22" 2>/dev/null; then probs+=("the host's ssh port answers"); fi
	v=$(x sed -n 's/^NoNewPrivs:\t//p' /proc/1/status 2>&1); [ "$v" = 1 ] || probs+=("NoNewPrivs of PID 1: $v")
	[ ${#probs[@]} = 0 ] && { echo same; return 0; }
	printf '%s\n' "${probs[@]}"; return 1
}

# The stand-in: its request log, and what it answers (a mode file, checked by a probe of
# another path, whose line is then dropped).
# Cut the log back to its first N lines, in place: nginx keeps the file open (sed -i would make
# a new one).
cutlog() { x sh -c 'head -n "$1" /var/log/stub.log > /var/log/stub.cut && cat /var/log/stub.cut > /var/log/stub.log && rm /var/log/stub.cut' _ "$1"; }
calls() { x sh -c '[ -f /var/log/stub.log ] && wc -l < /var/log/stub.log || echo 0' | tr -d ' '; }
lastline() { x tail -n 1 /var/log/stub.log 2>/dev/null; }
probe() { x curl -s --max-time 2 -o /dev/null -w '%{http_code}' https://api.anthropic.com/probe; }
# The probes (the old mode may answer a few until the reload is done) are cut from the log.
# The slow mode throttles even its headers: its probe times out (000).
mode() {
	local want n
	case $1 in ok) want=404 ;; 429) want=429 ;; redirect) want=302 ;; slow) want=000 ;; esac
	n=$(calls)
	x cp "/opt/cu/sandbox/https-stub/mode-$1.conf" /etc/nginx/cu-mode.conf && x nginx -s reload 2> /dev/null &&
		wait_for 10 '[ "$(probe)" = "$want" ]' && cutlog "$n"
}
# claude-usage as sbuser; its state; edits of it (as sbuser, so the file stays its own).
cu() { ux "$CU" "$@"; }
G=''
getj() { G=$(cu get 2> /dev/null); }
jqg() { printf '%s' "$G" | jq -e "$1" > /dev/null 2>&1; }
SD() { ux sh -c 'ls -d ~/.local/state/claude-usage/*/ | head -1'; }
state() { ux sh -c 'jq -r "$1" ~/.local/state/claude-usage/*/usage.json' _ "$1" 2>/dev/null; }
state_edit() {
	ux sh -c 'f=$(ls ~/.local/state/claude-usage/*/usage.json) && jq "$1" "$f" > ~/st.tmp && mv -f ~/st.tmp "$f"' _ "$1"
}
age() { state_edit ".last_attempt -= $1 | if .next_attempt then .next_attempt -= $1 else . end"; }
gap() { local d; d=$(state '.next_attempt - .last_attempt'); [[ $d =~ ^[0-9]+$ ]] && [ "$d" -ge "$1" ] && [ "$d" -le "$2" ]; }
# Wait until the second within the 30 s window is in [LO, HI].
window() { local i s; for ((i = 0; i < 200; i++)); do s=$(($(date +%s) % 30)); [ "$s" -ge "$1" ] && [ "$s" -le "$2" ] && return 0; sleep 0.2; done; return 1; }
uprop() { ux systemctl --user show "$1" -p "$2" --value; }
# New pids in the container (ns_last_pid) during a plain get, then during a get --refresh: equal
# when --refresh spawned nothing. Each step forks the same way (one cat, one claude-usage).
spawns() {
	ux sh -c 'p=/proc/sys/kernel/ns_last_pid; a=$(cat $p); "$1" get > /dev/null; b=$(cat $p)
		"$1" get --refresh > /dev/null; c=$(cat $p); echo $((b - a)) $((c - b))' _ "$CU"
}

# The network is the run lock, so it goes last: no other run can start while the rest goes.
down() {
	local pulled; pulled=$(docker network inspect "$NET" --format '{{index .Labels "cu-sandbox.pulled-buildkit"}}' 2>/dev/null)
	docker rm -f "$C" >/dev/null 2>&1
	docker buildx rm -f "$BUILDER" >/dev/null 2>&1
	docker rmi "$IMAGE" >/dev/null 2>&1
	[ "$pulled" = 1 ] && docker rmi "$BK" >/dev/null 2>&1
	docker network rm "$NET" >/dev/null 2>&1
	return 0
}
live_owner() {
	local pid st
	pid=$(docker network inspect "$NET" --format '{{index .Labels "cu-sandbox.owner"}}' 2>/dev/null) || return 0
	st=$(docker network inspect "$NET" --format '{{index .Labels "cu-sandbox.started"}}' 2>/dev/null)
	[ -n "$pid" ] && [ -n "$st" ] && [ "$(started "$pid")" = "$st" ] && echo "$pid"
}
for c in docker jq git; do command -v "$c" > /dev/null || die "$c not found"; done
docker info >/dev/null 2>&1 || die "docker does not answer"
if [ "${1:-}" = "--down" ]; then
	p=$(live_owner); [ -z "$p" ] || die "a sandbox run is still going (pid $p); its own exit cleans up"
	down; echo "sandbox removed"; exit 0
fi
[ $# = 0 ] || die "usage: sandbox/run.sh [--down]"
p=$(live_owner); [ -z "$p" ] || die "a sandbox run is going (pid $p)"
left="left by an earlier run (KEEP=1, or killed): sandbox/run.sh --down"
docker container inspect "$C" >/dev/null 2>&1 && die "container $C exists, $left"
docker buildx inspect "$BUILDER" >/dev/null 2>&1 && die "builder $BUILDER exists, $left"
docker image inspect "$BK" >/dev/null 2>&1 && PULLED_BK=0 || PULLED_BK=1
CTX=""
trap 'rm -rf "$CTX"; [ "$LOCKED" = 1 ] && [ "${KEEP:-0}" != 1 ] && down' EXIT
trap 'echo "interrupted" >&2; exit 130' INT TERM HUP
# The network is the run lock (the container is not attached to it): creating it is atomic.
if ! err=$(docker network create --internal --label "cu-sandbox.pulled-buildkit=$PULLED_BK" \
		--label "cu-sandbox.owner=$$" --label "cu-sandbox.started=$(started $$)" "$NET" 2>&1); then
	p=$(live_owner)
	[ -n "$p" ] && die "a sandbox run is going (pid $p)"
	docker network inspect "$NET" >/dev/null 2>&1 && die "network $NET exists, $left"
	die "cannot create network $NET: $err"
fi
LOCKED=1

echo "== build (the release tarball, then ubuntu:24.04 + systemd, curl, nginx, the repo's tracked files)"
CTX="$(mktemp -d)"
if [ -n "${RELEASE:-}" ]; then
	[ -f "$RELEASE" ] && [ ! -L "$RELEASE" ] || die "RELEASE=$RELEASE: not a file"
	cp "$RELEASE" "$CTX/release.tar.gz" || die "copying $RELEASE"
	info "RELEASE: $(basename "$RELEASE"), sha256 $(sha256sum < "$RELEASE" | cut -c1-64)"
else
	tb=$("$REPO/scripts/tarball.sh" "$CTX/dist") || die "scripts/tarball.sh"
	mv "$tb" "$CTX/release.tar.gz" && rmdir "$CTX/dist"
fi
mkdir -p "$CTX/cu"
(cd "$REPO" && git ls-files -z | xargs -0 -I{} cp -P --parents {} "$CTX/cu/") || die "copying the repo"
[ -f "$CTX/cu/systemd/claude-usage.service" ] || die "copying the repo: no systemd/claude-usage.service (not git-added?)"
cp "$REPO/sandbox/Dockerfile" "$CTX/"
docker buildx create --name "$BUILDER" --driver docker-container --driver-opt image="$BK" \
	--driver-opt memory=1g --driver-opt memory-swap=1g >/dev/null 2>&1 || die "builder"
out=$(timeout 900 docker buildx build --builder "$BUILDER" --load --build-arg "CERT_DATE=$(date -u +%F)" \
	-t "$IMAGE" "$CTX" 2>&1) || { echo "$out" | tail -20; die "build"; }
docker buildx rm -f "$BUILDER" >/dev/null 2>&1
[ "$PULLED_BK" = 1 ] && docker rmi "$BK" >/dev/null 2>&1
rm -rf "$CTX"; CTX=""

echo "== 0. safety (abort when the container could reach the host)"
docker create --name "$C" "${OPTS[@]}" "$IMAGE" "${INIT[@]}" >/dev/null || die "docker create"
chk "its whole HostConfig is the expected one, no mounts, no network (before it starts)" '[ "$(hostconfig_diff)" = same ]' 'hostconfig_diff'
[ "$fail" = 0 ] || { docker rm -f "$C" >/dev/null 2>&1; die "the safety check failed (the container is removed)"; }
docker start "$C" >/dev/null || die "docker start"
chk "inside: capabilities, OOM score, /proc/sys and /sys read-only, loopback only, the host out of reach, no new privileges" \
	'[ "$(inside_diff)" = same ]' 'inside_diff'
[ "$fail" = 0 ] || { docker rm -f "$C" >/dev/null 2>&1; die "the safety check failed (the container is removed)"; }

echo "== 1. systemd, the users, the stand-in"
chk "systemd is PID 1 and running (or degraded)" 'wait_for 60 "x systemctl is-system-running 2>/dev/null | grep -qE \"running|degraded\""' 'x systemctl --failed --no-legend'
chk "linger starts sbuser's user manager" 'wait_for 30 "x systemctl is-active --quiet user@$UID1.service"'
chk "systemctl --user answers" 'wait_for 20 "ux systemctl --user is-system-running 2>/dev/null | grep -qE \"running|degraded\""'
chk "sbuser2 (no linger) has no user manager" '! x systemctl is-active --quiet "user@$UID2.service"'
chk "the stand-in answers on https://api.anthropic.com with the test CA, over HTTP/2" \
	'wait_for 20 "[ \"\$(probe)\" = 404 ]" && x curl -s -o /dev/null -w "%{http_version}" https://api.anthropic.com/probe | grep -qx 2 && x sh -c ": > /var/log/stub.log"' \
	'x systemctl status nginx --no-pager | tail -5; x tail -3 /var/log/stub.err'
v=$(x curl --version | head -1); info "real curl: $v"

echo "== 2. install from the release tarball (install.sh), credentials"
EXP=$((($(date +%s) + 6 * 3600) * 1000))
creds="{\"claudeAiOauth\":{\"accessToken\":\"SECRET-TOKEN-SBX\",\"refreshToken\":\"SECRET-REFRESH-SBX\",\"expiresAt\":$EXP},\"mcpOAuth\":{\"s|1\":{\"accessToken\":\"SECRET-MCP-SBX\"}}}"
printf '%s' "$creds" | docker exec -i -u sbuser -w $H1 "$C" sh -ec 'mkdir -p ~/.claude; umask 077; cat > ~/.claude/.credentials.json'
unset creds
# sbuser's files (not folders) before any install: uninstall --purge must leave exactly these.
HOME0=$(ux sh -c 'find ~ \( -type f -o -type l \) | sort')
r=$(x sh -c 'cd /tmp && tar -xzf /opt/cu-rel/release.tar.gz && cd / && /tmp/claude-usage-*/install.sh' 2>&1); rc=$?
x sh -c 'rm -rf /tmp/claude-usage-*'
chk "install.sh as root: refused (exit 1), nothing installed for root" \
	'[ "$rc" = 1 ] && echo "$r" | grep -q "not as root" && ! x sh -c "test -e ~root/.local/bin/claude-usage"' 'echo "$r"'
r=$(x env HOME=$H1 /opt/cu-bin/claude-usage install 2>&1); rc=$?
chk "claude-usage install as root with sbuser's HOME (sudo -E style): refused, nothing of root's in sbuser's home" \
	'[ "$rc" = 1 ] && echo "$r" | grep -q "running as root" && [ -z "$(x find $H1 -user root)" ]' 'echo "$r"; x find $H1 -user root'
r=$(x sh -c 'cd / && /opt/cu-bin/claude-usage install' 2>&1); rc=$?
chk "claude-usage install as root in root's own home: refused, nothing installed" \
	'[ "$rc" = 1 ] && echo "$r" | grep -q "running as root" && ! x sh -c "test -e ~root/.local"' 'echo "$r"'
ux sh -ec 'mkdir ~/rel && tar -xzf /opt/cu-rel/release.tar.gz -C ~/rel'
REL=$(ux sh -c 'ls -d ~/rel/claude-usage-*-x86_64-linux')
r=$(ux sh -c 'cd /tmp && "$1/install.sh" --no-timer' _ "$REL" 2>&1); rc=$?
chk "install.sh from another folder, as sbuser, --no-timer: exit 0, installed, the units loaded, the timer disabled" \
	'[ "$rc" = 0 ] && ux cmp -s "$REL/claude-usage" "$CU" && [ "$(ux systemctl --user is-enabled claude-usage.timer 2>&1)" = disabled ] && [ "$(uprop claude-usage.service LoadState)" = loaded ] && echo "$r" | grep -q "^timer: off"' \
	'echo "$r"'
r=$(cu install --status 2>&1); rc=$?
chk "install --status: healthy, exit 0" '[ "$rc" = 0 ] && echo "$r" | grep -qx healthy' 'echo "$r"'
chk "the units load (systemd-analyze verify: nothing said)" \
	'[ -z "$(ux systemd-analyze --user verify ~/.config/systemd/user/claude-usage.service ~/.config/systemd/user/claude-usage.timer 2>&1)" ]' \
	'ux systemd-analyze --user verify ~/.config/systemd/user/claude-usage.service ~/.config/systemd/user/claude-usage.timer'

echo "== 3. the real curl"
cu fetch; rc=$?
getj
L=$(lastline)
chk "fetch through the real curl: exit 0, one call, the numbers" \
	'[ "$rc" = 0 ] && [ "$(calls)" = 1 ] && jqg ".error == null and .windows.five_hour == {\"pct\":30.4,\"resets_at\":1893456000} and .windows.seven_day.pct == 12"' 'echo "$G"; x cat /var/log/stub.log'
chk "the request: HTTP/2, GET /api/oauth/usage, the token as one Bearer header, the beta header" \
	'[ "$(echo "$L" | cut -d"|" -f1-6)" = "HTTP/2.0|GET|/api/oauth/usage|Bearer SECRET-TOKEN-SBX|oauth-2025-04-20|-" ]' 'echo "$L"'
chk "no config file: status names the system curl" 'cu status | grep -qx "curl: *\/usr\/bin\/curl"' 'cu status'
mode 429
age 40
cu fetch; rc=$?
getj
chk "429 from the real curl: rate_limited, Retry-After 120 read (the -w tail expanded), numbers kept" \
	'[ "$rc" = 0 ] && [ "$(calls)" = 2 ] && jqg ".error == \"rate_limited\" and .windows.five_hour.pct == 30.4" && [ "$(state .retry_after_seen)" = true ] && gap 120 122' \
	'echo "$G"; state .; x cat /var/log/stub.log'
mode redirect
age 200
cu fetch; rc=$?
getj
# The redirect points back at the stand-in: a followed one would be a second logged request.
chk "a redirect is not followed: http_error, one request only" \
	'[ "$rc" = 0 ] && [ "$(calls)" = 3 ] && jqg ".error == \"http_error\"" && lastline | grep -q "|302$"' 'echo "$G"; x cat /var/log/stub.log'
mode ok
state_edit '.next_attempt = null | .last_attempt -= 3700'
ux sh -c 'printf "header = \"X-Curlrc: 1\"\ntrace-ascii = \"/tmp/curlrc-trace\"\n" > ~/.curlrc'
ux env SSLKEYLOGFILE=/tmp/sslkeys "$CU" fetch; rc=$?
L=$(lastline)
chk "a ~/.curlrc and SSLKEYLOGFILE: neither reaches curl" \
	'[ "$rc" = 0 ] && [ "$(calls)" = 4 ] && [ "$(echo "$L" | cut -d"|" -f6)" = "-" ] && ! x test -e /tmp/curlrc-trace && ! x test -e /tmp/sslkeys' \
	'echo "$L"; x ls -la /tmp'
n0=$(calls)
ux env SSLKEYLOGFILE=/tmp/sslkeys curl -s -o /dev/null https://api.anthropic.com/probe
chk "(control: the same curl without claude-usage reads both)" \
	'x test -s /tmp/curlrc-trace && x test -s /tmp/sslkeys && lastline | cut -d"|" -f6 | grep -qx 1'
cutlog "$n0"; x rm -f /tmp/curlrc-trace /tmp/sslkeys
ux rm -f "$H1/.curlrc"

echo "== 4. the units: timer, hardening, journal"
r=$(cu install --timer 2>&1); rc=$?
chk "install --timer: through systemctl, the timer enabled and active, set for :00 and :30" \
	'[ "$rc" = 0 ] && echo "$r" | grep -q "^timer: enabled and started" && [ "$(ux systemctl --user is-enabled claude-usage.timer)" = enabled ] && [ "$(uprop claude-usage.timer ActiveState)" = active ] && uprop claude-usage.timer TimersCalendar | grep -q "OnCalendar=\*-\*-\* \*:\*:00,30"' \
	'ux systemctl --user status claude-usage.timer --no-pager'
n0=$(calls)
chk "the timer fires at the next :00/:30 and the fetch calls (a boundary has passed)" \
	'wait_for 40 "[ \$(calls) -gt $n0 ]"' 'ux systemctl --user status claude-usage.service --no-pager; x journalctl --no-pager -n 20 -o cat'
wait_for 10 '[ "$(uprop claude-usage.service ActiveState)" = inactive ]'
chk "the service run ended well (Result=success, exit 0)" \
	'[ "$(uprop claude-usage.service Result)" = success ] && [ "$(uprop claude-usage.service ExecMainStatus)" = 0 ]' \
	'ux systemctl --user status claude-usage.service --no-pager'
getj
chk "the timer's fetch recorded new numbers (get: no error, fetched_at at that boundary)" \
	'jqg ".error == null and .fetched_at >= $(($(date +%s) - 45)) and .windows.five_hour.pct == 30.4"' \
	'echo "$G"; date +%s; state .; x tail -n 3 /var/log/stub.log'
chk "the unit as read back: TimeoutStartSec 30 s, LogLevelMax notice (5), UMask 0077" \
	'[ "$(uprop claude-usage.service TimeoutStartUSec)" = 30s ] && [ "$(uprop claude-usage.service LogLevelMax)" = 5 ] && [ "$(uprop claude-usage.service UMask)" = 0077 ]' \
	'ux systemctl --user show claude-usage.service -p TimeoutStartUSec,LogLevelMax,UMask'
# What the service's processes really get: a probe as ExecStartPre (same cgroup, same
# restrictions) through a drop-in, and the same probe run by the same user manager with no
# hardening (control). The container's PID 1 already has no-new-privileges and Docker's
# seccomp filter, so NoNewPrivs proves nothing here; more seccomp filters, unshare refused and
# the cgroup limits do.
ux sh -ec 'cat > ~/probe.sh <<"P"
#!/bin/sh
{ sed -n "s/^Seccomp_filters:[[:space:]]*/filters /p" /proc/self/status
  unshare -U true 2> /dev/null; echo "unshare $?"
  g=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
  echo "pids $(cat "$g/pids.max" 2>&1)"; echo "memory $(cat "$g/memory.max" 2>&1)"
  echo "umask $(umask)"; } > "$1"
P
	chmod 755 ~/probe.sh; d=~/.config/systemd/user/claude-usage.service.d; mkdir -p "$d"
	printf "[Service]\nExecStartPre=%%h/probe.sh %%t/cu-hard\n" > "$d/cu-probe.conf"
	systemctl --user daemon-reload; systemctl --user start claude-usage.service
	rm -r "$d"; systemctl --user daemon-reload
	systemd-run --user --quiet --wait --collect ~/probe.sh "$XDG_RUNTIME_DIR/cu-plain"'
HARD=$(ux cat $RUN1/cu-hard 2>&1) PLAIN=$(ux cat $RUN1/cu-plain 2>&1)
ux rm -f "$H1/probe.sh" "$RUN1/cu-hard" "$RUN1/cu-plain"
info "probe, hardened: ${HARD//$'\n'/; }"
info "probe, plain:    ${PLAIN//$'\n'/; }"
hv() { printf '%s\n' "$1" | sed -n "s/^$2 //p"; }
chk "the hardening as applied: more seccomp filters than an unhardened unit, user namespaces refused (control: allowed), TasksMax 32, MemoryMax 128 MiB, umask 0077" \
	'[ "$(hv "$HARD" filters)" -gt "$(hv "$PLAIN" filters)" ] && [ "$(hv "$HARD" unshare)" != 0 ] && [ "$(hv "$PLAIN" unshare)" = 0 ] && [ "$(hv "$HARD" pids)" = 32 ] && [ "$(hv "$HARD" memory)" = 134217728 ] && [ "$(hv "$HARD" umask)" = 0077 ]' \
	'echo "hardened: $HARD"; echo "plain: $PLAIN"'
# A reader at the same boundary: get --refresh every 0.1 s from :26 or :27 until 3 s after the
# boundary (the timer fires within AccuracySec=1s of it). One call in that window.
chk "(the reader loop starts before the boundary)" 'window 26 27'
n0=$(calls)
ux sh -c 'end=$(( ($(date +%s) / 30 + 1) * 30 + 3 )); while [ "$(date +%s)" -lt "$end" ]; do "$1" get --refresh > /dev/null; sleep 0.1; done' _ "$CU"
wait_for 10 '[ "$(uprop claude-usage.service ActiveState)" = inactive ]'
sleep 1
chk "readers and the timer at one boundary: one call" '[ "$(($(calls) - n0))" = 1 ]' 'x cat /var/log/stub.log'
# Routine runs leave no journal lines: neither the service's nor the user manager's about it
# (Starting, Finished, at info). Control: the same unit with LogLevelMax=debug in a drop-in.
x journalctl --sync
mgr=$(x journalctl --no-pager -o cat USER_UNIT=claude-usage.service | wc -l)
own=$(x journalctl --no-pager -o cat _SYSTEMD_USER_UNIT=claude-usage.service | wc -l)
ux sh -ec 'd=~/.config/systemd/user/claude-usage.service.d; mkdir -p "$d"
	printf "[Service]\nLogLevelMax=debug\n" > "$d/cu-sandbox.conf"
	systemctl --user daemon-reload; systemctl --user start claude-usage.service
	rm -r "$d"; systemctl --user daemon-reload'
x journalctl --sync
ctl=$(x journalctl --no-pager -o cat USER_UNIT=claude-usage.service | wc -l)
chk "routine runs leave no journal line: the fetch says nothing, LogLevelMax hides the manager's (control: with debug, they are there)" \
	'[ "$mgr" = 0 ] && [ "$own" = 0 ] && [ "$ctl" -gt 0 ] && [ "$(uprop claude-usage.service LogLevelMax)" = 5 ]' \
	'echo "manager lines $mgr, service lines $own, with debug $ctl"; x journalctl --no-pager -o cat USER_UNIT=claude-usage.service | tail -5'
# A refusal under the unit: its <4> line is kept, at warning priority.
ux systemctl --user set-environment CLAUDE_CONFIG_DIR=relative/path
ux systemctl --user start claude-usage.service 2>/dev/null
ux systemctl --user unset-environment CLAUDE_CONFIG_DIR
ux systemctl --user reset-failed claude-usage.service 2>/dev/null
chk "a refusal under the unit: its line in the journal at warning priority (LogLevelMax keeps it)" \
	'x journalctl --no-pager -o json _SYSTEMD_USER_UNIT=claude-usage.service | jq -r "select(.MESSAGE | test(\"fetch refused\")) | .PRIORITY" | grep -qx 4' \
	'x journalctl --no-pager -o json _SYSTEMD_USER_UNIT=claude-usage.service | tail -3'
r=$(cu install --no-timer 2>&1); rc=$?
chk "install --no-timer: through systemctl, the timer disabled and stopped" \
	'[ "$rc" = 0 ] && [ "$(ux systemctl --user is-enabled claude-usage.timer 2>&1)" = disabled ] && [ "$(uprop claude-usage.timer ActiveState)" = inactive ]' 'echo "$r"'

echo "== 4b. resources (the timer runs one fetch every 30 s, for hours)"
# One run as sbuser under GNU time: "wall_ms cpu_ms rss_kb", or nothing when time failed (the
# file is removed first, so an old one never counts). wait4's rusage counts the waited-for
# children too, so a fetch's numbers include its curl. Bounds are generous (a loaded host); the
# info lines give the numbers.
measure() {
	ux sh -c 'rm -f /tmp/cu-time; /usr/bin/time -f "%e %U %S %M" -o /tmp/cu-time "$@" > /dev/null 2>&1; tail -n 1 /tmp/cu-time' _ "$@" 2> /dev/null |
		awk 'NF == 4 && $4 > 0 { printf "%d %d %d\n", $1 * 1000, ($2 + $3) * 1000, $4 }'
}
# The largest of each column over the lines on stdin, and how many lines there were.
maxcols() { awk 'NF == 3 { n++; for (i = 1; i <= 3; i++) if ($i > m[i]) m[i] = $i } END { printf "%d %d %d %d\n", m[1], m[2], m[3], n }'; }
# One run under strace -c, children included: "syncs renames writes ran". writes counts write,
# pwrite64, ftruncate and truncate on any fd (stdout too); ran is 1 when strace left its summary
# (removed first), so a strace that did not run never reads as "no syscalls".
syscalls() {
	ux sh -c 'rm -f /tmp/cu-strace
		strace -f -qq -c -o /tmp/cu-strace -e trace=fsync,fdatasync,rename,renameat,renameat2,write,pwrite64,ftruncate,truncate "$@" > /dev/null 2>&1
		[ -f /tmp/cu-strace ] && ran=1 || ran=0
		awk -v ran=$ran "/sync\$/ { s += \$4 } /rename/ { r += \$4 } / (write|pwrite64|ftruncate|truncate)\$/ { w += \$4 } END { printf \"%d %d %d %d\\n\", s, r, w, ran }" /tmp/cu-strace 2> /dev/null ||
			echo "0 0 0 $ran"' _ "$@"
}
due() { state_edit '.next_attempt = null | .last_attempt -= 40'; }
mode ok
sizes0=$(ux sh -c 'find ~/.local/state/claude-usage | wc -l')
dues='' idles=''
n0=$(calls)
for _ in 1 2 3 4 5; do
	window 3 18
	due
	dues+=$(measure "$CU" fetch)$'\n'
	idles+=$(measure "$CU" fetch)$'\n'
done
n1=$(calls)
read -r dw dc dr dn <<< "$(maxcols <<< "$dues")"
read -r iw ic ir icount <<< "$(maxcols <<< "$idles")"
info "a due fetch (real curl, TLS to the stand-in), worst of 5: ${dw} ms, CPU ${dc} ms, peak ${dr} KiB"
info "a run with nothing due, worst of 5: ${iw} ms, CPU ${ic} ms, peak ${ir} KiB"
chk "the measured runs: 5 calls, one per due run, none when nothing was due" '[ $((n1 - n0)) = 5 ]'
chk "every run measured (5 due, 5 with nothing due, each with a peak above 0)" '[ "$dn" = 5 ] && [ "$icount" = 5 ]' \
	'echo "due: $dues"; echo "idle: $idles"'
chk "a due fetch is small: under 3 s, CPU under 300 ms, peak under 32 MiB (curl included)" \
	'[ "$dn" = 5 ] && [ "$dw" -lt 3000 ] && [ "$dc" -lt 300 ] && [ "$dr" -lt 32768 ]' 'echo "$dues"'
chk "a run with nothing due is tiny: CPU under 30 ms, peak under 8 MiB" \
	'[ "$icount" = 5 ] && [ "$ic" -lt 30 ] && [ "$ir" -lt 8192 ]' 'echo "$idles"'
g=$(for _ in 1 2 3; do measure "$CU" get; done | maxcols)
read -r gw gc gr gn <<< "$g"
info "get, worst of 3: ${gw} ms, CPU ${gc} ms, peak ${gr} KiB"
chk "get is tiny: CPU under 30 ms, peak under 8 MiB (3 runs measured)" \
	'[ "$gn" = 3 ] && [ "$gc" -lt 30 ] && [ "$gr" -lt 8192 ]' 'echo "$g"'
window 3 18
due
read -r dsync dren dwr dran <<< "$(syscalls "$CU" fetch)"
read -r isync iren iwr iran <<< "$(syscalls "$CU" fetch)"
info "syscalls: a due fetch ${dsync} fsync, ${dren} rename, ${dwr} write; nothing due ${isync} fsync, ${iren} rename, ${iwr} write"
chk "a run with nothing due writes nothing (no write, truncate, fsync or rename on any fd)" \
	'[ "$iran" = 1 ] && [ "$isync" = 0 ] && [ "$iren" = 0 ] && [ "$iwr" = 0 ]'
chk "a due fetch: two state writes (the record before the call, the result), no fsync" \
	'[ "$dran" = 1 ] && [ "$dren" = 2 ] && [ "$dsync" = 0 ] && [ "$dwr" -gt 0 ]'
sizes1=$(ux sh -c 'find ~/.local/state/claude-usage | wc -l')
bytes=$(ux sh -c 'stat -c %s ~/.local/state/claude-usage/*/usage.json')
chk "the state does not grow: the same files after 11 fetches, usage.json under 4 KiB" \
	'[ "$sizes1" = "$sizes0" ] && [ "$bytes" -lt 4096 ]' 'echo "$sizes0 -> $sizes1 files, $bytes bytes"'
info "per day with the timer and a valid token (2880 due runs): CPU about $((dc * 2880 / 1000)) s at most"

echo "== 5. what needs root"
r=$(x env HOME=$H1 CLAUDE_CONFIG_DIR=$H1/.claude "$CU" fetch 2>&1); rc=$?
chk "as root (sudo -E style): refused, exit 1, one <4> line, nothing of root's in sbuser's home" \
	'[ "$rc" = 1 ] && [ "$r" = "<4>claude-usage: fetch refused: running as root" ] && [ -z "$(x find $H1 -user root)" ]' 'echo "$r"; x find $H1 -user root'
age 3700
x chown root:root $H1/.claude/.credentials.json && x chmod 644 $H1/.claude/.credentials.json
n0=$(calls)
r=$(cu fetch 2>&1); rc=$?
r2=$(cu fetch 2>&1)
read -r sp_plain sp_refresh <<< "$(spawns)"
sleep 2
chk "credentials owned by another user: refused once (exit 1, one <4> line), no call, get --refresh starts nothing" \
	'[ "$rc" = 1 ] && [ "$r" = "<4>claude-usage: fetch refused: the credentials file is not yours" ] && [ -z "$r2" ] && [ "$(calls)" = "$n0" ] && [ "$(state .refusal)" = credentials_not_yours ] && [ "$sp_refresh" = "$sp_plain" ]' \
	'echo "$r / $r2 / new pids: get $sp_plain, get --refresh $sp_refresh"; state .'
x chown sbuser:sbuser $H1/.claude/.credentials.json && x chmod 600 $H1/.claude/.credentials.json
read -r sp_plain sp_refresh <<< "$(spawns)"
wait_for 10 '[ "$(calls)" = $((n0 + 1)) ]'
wait_for 10 '[ "$(state .pending)" = false ]'
chk "back to the user's own: get --refresh starts the fetch (more pids: the control), called, the refusal cleared" \
	'[ "$sp_refresh" -gt "$sp_plain" ] && [ "$(calls)" = $((n0 + 1)) ] && [ "$(state .refusal)" = null ]' \
	'echo "new pids: get $sp_plain, get --refresh $sp_refresh"; state .'
K=$(SD)
age 3700
# Open to all (777, 666), so only the owner check can refuse them, not a permission error.
x sh -c "chown root:root '$K' && chmod 777 '$K'"
getj
n0=$(calls)
cu fetch 2> /dev/null; rc=$?
chk "a state folder owned by another user (mode 777): get says state_unusable, fetch exit 1, no call" \
	'jqg ".error == \"state_unusable\" and .fetched_at == null" && [ "$rc" = 1 ] && [ "$(calls)" = "$n0" ]' 'echo "$G"'
x sh -c "chown sbuser:sbuser '$K' && chmod 700 '$K' && chown root:root '$K/usage.json' && chmod 666 '$K/usage.json'"
getj
cu fetch 2> /dev/null; rc=$?
chk "a usage.json owned by another user (mode 666): get says state_unusable, fetch exit 1, no call" \
	'jqg ".error == \"state_unusable\" and .fetched_at == null" && [ "$rc" = 1 ] && [ "$(calls)" = "$n0" ]' 'echo "$G"'
x sh -c "chown sbuser:sbuser '$K/usage.json' && chmod 600 '$K/usage.json'"
getj
chk "(control: both the user's own again: get shows the numbers)" 'jqg ".windows.five_hour.pct == 30.4"' 'echo "$G"'
x sh -c "mount --bind '$K' '$K' && mount -o remount,bind,ro '$K'"
cu fetch 2> /dev/null; rc=$?
getj
chk "a read-only state folder: fetch exit 1, no call; get still shows the numbers" \
	'[ "$rc" = 1 ] && [ "$(calls)" = "$n0" ] && jqg ".windows.five_hour.pct == 30.4"' 'echo "$G"'
x umount "$K"
# A full disk: a 64 KiB tmpfs over the folder, its files copied in, then filled.
x sh -ec "cp -a '$K' /tmp/cu-k; mount -t tmpfs -o size=64k,mode=700,uid=$UID1,gid=$UID1 tmpfs '$K'
	cp -a /tmp/cu-k/. '$K/'; rm -r /tmp/cu-k; dd if=/dev/zero of='$K/filler' bs=4k 2> /dev/null || true"
r=$(cu fetch 2>&1); rc=$?
getj
chk "a full state folder: fetch exit 1 (write_failed), no call; get still shows the numbers" \
	'[ "$rc" = 1 ] && [ "$r" = "<4>claude-usage: fetch refused: the state could not be written (read-only or full?)" ] && [ "$(calls)" = "$n0" ] && jqg ".windows.five_hour.pct == 30.4"' \
	'echo "$r"; echo "$G"; x df "$K"'
x umount "$K"
cu fetch; rc=$?
chk "(control: the folder usable again, the same fetch calls: it was due all along)" '[ "$rc" = 0 ] && [ "$(calls)" = $((n0 + 1)) ]'

echo "== 6. install and uninstall"
# An upgrade: the same release with a byte appended to its program (an ELF file runs with
# trailing data) and new sums.
ux sh -ec 'cp -r "$1" ~/rel/next && cd ~/rel/next && printf x >> claude-usage && sha256sum claude-usage install.sh > SHA256SUMS' _ "$REL"
r=$(ux sh -c 'cd / && ~/rel/next/install.sh' 2>&1); rc=$?
chk "an upgrade without a flag: exit 0, the new program in place and running, the timer still off" \
	'[ "$rc" = 0 ] && ux cmp -s "$H1/rel/next/claude-usage" "$CU" && cu get | jq -e .schema > /dev/null && [ "$(ux systemctl --user is-enabled claude-usage.timer 2>&1)" = disabled ] && ux grep -qx "timer off" "$H1/.local/share/claude-usage/manifest"' \
	'echo "$r"'
r=$(ux sh -c 'cd / && "$1/install.sh"' _ "$REL" 2>&1); rc=$?
chk "and back to the first release: exit 0, still off" \
	'[ "$rc" = 0 ] && ux cmp -s "$REL/claude-usage" "$CU" && [ "$(ux systemctl --user is-enabled claude-usage.timer 2>&1)" = disabled ]' 'echo "$r"'
# Kills: install --timer killed at points up to 0.3 s in (its systemctl calls), then run again.
# The delay is bash's own read -t on a FIFO (no forked sleep).
hits=0 good=1
for d in 0.001 0.003 0.006 0.01 0.02 0.04 0.07 0.1 0.15 0.2 0.3; do
	h=$(ux bash -c 'f=$(mktemp -u); mkfifo "$f"; exec 5<> "$f"; rm "$f"
		"$1" install --timer > /dev/null 2>&1 & p=$!
		read -rt "$2" -u 5 _; kill -9 "$p" 2> /dev/null; wait "$p" 2> /dev/null
		grep -qx "pending install" ~/.local/share/claude-usage/manifest && echo hit; true' _ "$CU" "$d")
	[ "$h" = hit ] && hits=$((hits + 1))
	r=$(cu install --timer 2>&1) && cu install --status > /dev/null && [ "$(uprop claude-usage.timer ActiveState)" = active ] || good=0
	cu install --no-timer > /dev/null 2>&1 || good=0
done
info "install --timer killed at 11 points: $hits landed mid-install"
chk "install --timer killed at 11 points (at least one mid-install): the next run finishes it, healthy, the timer active" \
	'[ "$good" = 1 ] && [ "$hits" -gt 0 ]' 'echo "$r"'
# No user manager: sbuser2 (no linger) installs; turning linger on starts the timer.
x sh -ec 'mkdir /home/sbuser2/rel && tar -xzf /opt/cu-rel/release.tar.gz -C /home/sbuser2/rel && chown -R sbuser2:sbuser2 /home/sbuser2/rel'
r=$(docker exec -u sbuser2 -w / "$C" sh -c '/home/sbuser2/rel/claude-usage-*/install.sh' 2>&1); rc=$?
chk "install with no user manager (sbuser2): exit 0, the files, the timer linked, when it starts and linger said" \
	'[ "$rc" = 0 ] && echo "$r" | grep -q "starts with your systemd user manager" && echo "$r" | grep -q "linger is off" && x test -L /home/sbuser2/.config/systemd/user/timers.target.wants/claude-usage.timer && x test -x /home/sbuser2/.local/bin/claude-usage' \
	'echo "$r"'
x loginctl enable-linger sbuser2
u2() { docker exec -u sbuser2 -e XDG_RUNTIME_DIR=/run/user/$UID2 -w /home/sbuser2 "$C" "$@"; }
chk "linger on, sbuser2's manager starts: the timer is active with no systemctl enable" \
	'wait_for 30 "x systemctl is-active --quiet user@$UID2.service" && wait_for 20 "u2 systemctl --user is-active --quiet claude-usage.timer"'
r=$(u2 /home/sbuser2/.local/bin/claude-usage uninstall --purge 2>&1); rc=$?
chk "sbuser2's uninstall --purge through systemctl: exit 0, the timer gone" \
	'[ "$rc" = 0 ] && ! u2 systemctl --user is-active --quiet claude-usage.timer && [ -z "$(x find /home/sbuser2/.config/systemd/user /home/sbuser2/.local -type f)" ]' 'echo "$r"'
# Group-writable folders (sbuser2, its timer off): its private group alone may write; a group
# with a member listed, a group named otherwise, and an ACL are refused. And a HOME that is not
# the passwd home (the units would never be read, %h names another program) is refused.
CU2=$(x sh -c 'ls -d /home/sbuser2/rel/claude-usage-*-x86_64-linux')/claude-usage
x sh -ec 'mkdir -p /home/sbuser2/.local/bin && chown sbuser2:sbuser2 /home/sbuser2/.local /home/sbuser2/.local/bin && chmod 775 /home/sbuser2/.local/bin'
r=$(u2 "$CU2" install --no-timer 2>&1); rc=$?
chk "a ~/.local/bin writable by the user's private group (775): accepted" '[ "$rc" = 0 ]' 'echo "$r"'
u2 "$CU2" uninstall > /dev/null 2>&1
x usermod -aG sbuser2 sbuser
r=$(u2 "$CU2" install --no-timer 2>&1); rc=$?
x gpasswd -d sbuser sbuser2 > /dev/null
chk "the same, another user listed in that group: refused" '[ "$rc" = 1 ] && echo "$r" | grep -q "only you can write"' 'echo "$r"'
x setfacl -m u:sbuser:rwx /home/sbuser2/.local/bin
r=$(u2 "$CU2" install --no-timer 2>&1); rc=$?
acl_n=$(x getfacl -cp /home/sbuser2/.local/bin | grep -c "^user:sbuser:")
x setfacl -b /home/sbuser2/.local/bin
chk "the same, an ACL letting another user write: refused" '[ "$acl_n" = 1 ] && [ "$rc" = 1 ] && echo "$r" | grep -q "only you can write"' 'echo "$r"'
x chgrp sbshared /home/sbuser2/.local/bin
r=$(docker exec -u sbuser2:sbshared -w /home/sbuser2 "$C" "$CU2" install --no-timer 2>&1); rc=$?
x chgrp sbuser2 /home/sbuser2/.local/bin
chk "the same, the effective group one not named as the user: refused" '[ "$rc" = 1 ] && echo "$r" | grep -q "only you can write"' 'echo "$r"'
r=$(u2 sh -c 'mkdir -p /tmp/h2 && HOME=/tmp/h2 "$1" install --no-timer' _ "$CU2" 2>&1); rc=$?
chk "install with a HOME that is not the passwd home: refused, nothing made there" \
	'[ "$rc" = 1 ] && echo "$r" | grep -q "not your home in the passwd file" && [ -z "$(x ls -A /tmp/h2)" ]' 'echo "$r"'
x rm -rf /tmp/h2 /home/sbuser2/.local
# uninstall --purge while a fetch runs (the stand-in answers at 1 byte/s: the fetch holds its
# lock for 5 s) and readers keep calling get --refresh: it waits, then removes everything; no
# fetch makes the state again. The timer is off: uninstall stops the service first, which would
# end a fetch the timer started instead of waiting for it.
mode slow
age 3700
ux sh -c 'setsid "$1" fetch < /dev/null > /dev/null 2>&1 &' _ "$CU"
wait_for 5 '! ux sh -c "flock -n ~/.local/state/claude-usage/*/.lock true"'
ux sh -c 'setsid sh -c '"'"'end=$(($(date +%s) + 12)); while [ "$(date +%s)" -lt "$end" ]; do "$0" get --refresh; sleep 0.1; done'"'"' "$1" < /dev/null > /dev/null 2>&1 &' _ "$CU"
s=$(date +%s)
r=$(cu uninstall --purge 2>&1); rc=$?
e=$(($(date +%s) - s))
chk "uninstall --purge during a running fetch, readers racing it: waits for the fetch, exit 0" '[ "$rc" = 0 ] && [ "$e" -ge 2 ]' 'echo "$r (took $e s)"'
sleep 13
chk "after it: no claude-usage process, no state made again, the units unknown to the manager" \
	'! x pgrep -u sbuser -f claude-usage > /dev/null && ! ux test -e "$H1/.local/state/claude-usage" && [ "$(uprop claude-usage.timer LoadState)" = not-found ]' \
	'x pgrep -a -u sbuser -f claude-usage; ux ls -la "$H1/.local/state"; uprop claude-usage.timer LoadState'
ux rm -rf "$H1/rel"
HOME1=$(ux sh -c 'find ~ \( -type f -o -type l \) | sort')
chk "uninstall --purge left exactly sbuser's files from before the install (empty folders aside)" '[ "$HOME1" = "$HOME0" ]' \
	'diff <(echo "$HOME0") <(echo "$HOME1")'
mode ok

echo "== 7. leaks"
chk "the journal holds no token" '[ "$(x sh -c "journalctl -o export | grep -c SECRET")" = 0 ]'
# Every file in the container but /proc, /sys, /dev (/dev/shm is searched) and the repo's copy.
# Allowed: the credentials, the stand-in's log and its mode file.
leaks() {
	x sh -c 'find / /dev/shm \( -path /proc -o -path /sys -o -path /dev -o -path /opt/cu \) -prune -o -type f -print0 |
		xargs -0 grep -lE "SECRET-(TOKEN|REFRESH|MCP)-SBX" 2> /dev/null' |
		grep -vxF -e "$H1/.claude/.credentials.json" -e /var/log/stub.log -e /etc/nginx/cu-mode.conf
}
chk "no test token in any file but the credentials (and the stand-in's log and mode file)" '[ -z "$(leaks)" ]' 'leaks'
chk "sbuser2 (no user manager): get still answers" \
	'docker exec -u sbuser2 "$C" /opt/cu-bin/claude-usage get | jq -e ".error == \"no_credentials\"" > /dev/null'

echo "== teardown"
if [ "${KEEP:-0}" = 1 ]; then
	info "KEEP=1: the container $C and image $IMAGE stay (sandbox/run.sh --down removes them)"
else
	down
	LOCKED=0
	chk "nothing left: container, image, builder, network" \
		'! docker container inspect "$C" >/dev/null 2>&1 && ! docker image inspect "$IMAGE" >/dev/null 2>&1 && ! docker buildx inspect "$BUILDER" >/dev/null 2>&1 && ! docker network inspect "$NET" >/dev/null 2>&1'
fi
echo "sandbox: $pass passed, $fail failed"
[ "$fail" = 0 ] || printf '  failed: %s\n' "${FAILED[@]}"
[ "$fail" = 0 ]
