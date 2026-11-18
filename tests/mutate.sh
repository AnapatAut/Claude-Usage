#!/usr/bin/env bash
# Mutation tests: each safeguard in tests/mutants.txt is broken, once, in a copy of the working
# tree, and the check that names it must fail. A mutant whose check still passes (survived),
# that does not apply exactly once, or does not build, fails the run.
#   tests/mutate.sh               every mutant (sandbox ones last, one at a time)
#   tests/mutate.sh NAME…         only those
#   JOBS=N tests/mutate.sh        N copies at once (default 2: each runs a 1 GiB test container)
#   tests/mutate.sh --check       only check that every mutant applies exactly once
# Each copy gets the working tree (tracked and untracked files git does not ignore) and a copy of
# ./target, so only the crate itself is rebuilt; scripts/cargo, tests/cli.sh (CU_ONLY=the case)
# and sandbox/run.sh then run in that copy as they do here. Run it after tests/cli.sh passes:
# a check that fails without any mutant proves nothing here. Logs: target/mutate/<name>.log.
#
# tests/mutants.txt: blocks separated by blank lines, # comments.
#   name:   a-short-name
#   file:   path in the repo
#   from:   a line of the text to replace (several from: lines are joined by newlines)
#   to:     its replacement (several lines likewise; none at all deletes the text)
#           (another file:, from:, to: group adds a second edit, for a safeguard of two layers)
#   suite:  unit | cli CASE | sandbox
#   fails:  the check that must fail: a unit test's name (module::tests::name), or the start of a
#           cli.sh or sandbox/run.sh check's name that no other check of the run starts with
# A from:/to: value is the line after "from: " / "to: " exactly (leading spaces kept).
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIST=$REPO/tests/mutants.txt
OUT=$REPO/target/mutate
JOBS=${JOBS:-2}
die() { echo "tests/mutate.sh: $*" >&2; exit 2; }
[[ $JOBS =~ ^[1-8]$ ]] || die "JOBS must be 1 to 8"
[ -d "$REPO/target/release" ] || die "build first (scripts/cargo build --release; scripts/cargo test)"

# Parse the list into parallel arrays. A mutant's edits are joined by RS (\x1e), and an edit's
# file, from and to by US (\x1f): neither is in the source text.
RS=$'\x1e' US=$'\x1f'
names=() edits=() suites=() fails=()
edit() { # ends the edit being read, if any
	[ -n "$f$fr$to" ] || return 0
	[ -n "$f" ] && [ -n "$fr" ] || die "mutant $n: an edit needs file and from"
	ed=${ed:+$ed$RS}$f$US$fr$US$to
	f='' fr='' to='' has_to=0
}
flush() {
	[ -n "$n" ] || return 0
	edit
	[ -n "$ed" ] && [ -n "$su" ] && [ -n "$fa" ] || die "mutant $n: file, from, suite and fails are needed"
	case $su in unit | sandbox | "cli "[a-z_]*) ;; *) die "mutant $n: suite $su" ;; esac
	for x in "${names[@]}"; do [ "$x" != "$n" ] || die "mutant $n twice"; done
	names+=("$n") edits+=("$ed") suites+=("$su") fails+=("$fa")
	n='' ed='' su='' fa=''
}
n='' f='' fr='' to='' ed='' su='' fa='' has_to=0 lineno=0
while IFS= read -r line || [ -n "$line" ]; do
	lineno=$((lineno + 1))
	case $line in
	'#'*) ;;
	'') flush ;;
	'name: '*) flush; n=${line#name: } ;;
	'file: '*) edit; f=${line#file: } ;;
	'from: '* | 'from:') v=${line#from:}; v=${v# }; fr=${fr:+$fr$'\n'}$v ;;
	'to: '* | 'to:') v=${line#to:}; v=${v# }; if [ "$has_to" = 1 ]; then to=$to$'\n'$v; else to=$v has_to=1; fi ;;
	'suite: '*) su=${line#suite: } ;;
	'fails: '*) fa=${line#fails: } ;;
	*) die "tests/mutants.txt:$lineno: $line" ;;
	esac
done < "$LIST"
flush
[ ${#names[@]} -gt 0 ] || die "no mutants"

check_only=0
if [ "${1:-}" = --check ]; then check_only=1; shift; fi
want=("$@")
for w in "${want[@]}"; do
	found=0
	for x in "${names[@]}"; do [ "$x" = "$w" ] && found=1; done
	[ "$found" = 1 ] || die "unknown mutant: $w"
done
picked() { [ ${#want[@]} = 0 ] && return 0; local w; for w in "${want[@]}"; do [ "$w" = "$1" ] && return 0; done; return 1; }

mkdir -p "$OUT"
# One mutant: copy, apply, build, run its suite, judge. Prints one result line.
one() {
	local i=$1 name=${names[$1]} w log res
	w=$(mktemp -d "${TMPDIR:-/tmp}/cu-mutant.XXXXXX")
	log=$OUT/$name.log
	: > "$log"
	(cd "$REPO" && git ls-files -co --exclude-standard -z | xargs -0 cp --parents -t "$w") &&
		mkdir "$w/target" && cp -a "$REPO/target/release" "$REPO/target/debug" "$w/target/" 2> /dev/null
	res=$(apply_all "$w" "${edits[$i]}") || { finish "$w" "NOT APPLIED  $name ($res)"; return; }
	# sandbox/run.sh copies the git-tracked files of the copy: make the copy a repo of its own.
	(cd "$w" && git init -q && git add -A && git -c user.name=m -c user.email=0+m@users.noreply.github.com commit -qm m) >> "$log" 2>&1
	if ! (cd "$w" && scripts/cargo build --release --locked) >> "$log" 2>&1; then
		finish "$w" "NO BUILD     $name"
		return
	fi
	local suite=${suites[$i]} fa=${fails[$i]} out
	case $suite in
	unit)
		out=$(cd "$w" && scripts/cargo test --locked 2>&1)
		echo "$out" >> "$log"
		if grep -qxF "test $fa ... FAILED" <<< "$out"; then res="caught       $name"
		else res="SURVIVED     $name (test $fa did not fail)"; fi
		;;
	cli\ * | sandbox)
		if [ "$suite" = sandbox ]; then out=$(cd "$w" && sandbox/run.sh 2>&1)
		else out=$(cd "$w" && CU_ONLY=${suite#cli } tests/cli.sh 2>&1); fi
		echo "$out" >> "$log"
		case $(judge "$out" "$fa") in
		caught) res="caught       $name" ;;
		survived) res="SURVIVED     $name" ;;
		many) res="NO CHECK     $name (more than one check starts with: $fa)" ;;
		*) res="NO CHECK     $name (no check named: $fa; or the run aborted)" ;;
		esac
		;;
	esac
	finish "$w" "$res"
}
# The one check of OUT whose name starts with FA: caught (it failed), survived, none or many.
judge() {
	local hits n
	hits=$(awk -v a="  ok    $2" -v b="  FAIL  $2" 'index($0, a) == 1 || index($0, b) == 1' <<< "$1")
	n=$(grep -c . <<< "$hits")
	if [ "$n" = 0 ]; then echo none
	elif [ "$n" != 1 ]; then echo many
	elif [[ $hits == "  FAIL"* ]]; then echo caught
	else echo survived; fi
}
finish() {
	rm -rf "$1"
	echo "$2"
}
# Replaces the one occurrence of FROM in FILE with TO (literally: quoted patterns).
apply() {
	local file=$1 from=$2 to=$3 text rest
	[ -f "$file" ] || { echo "no file"; return 1; }
	IFS= read -r -d '' text < "$file"
	rest=${text#*"$from"}
	[ "$rest" != "$text" ] || { echo "text not found"; return 1; }
	[[ $rest != *"$from"* ]] || { echo "text found more than once"; return 1; }
	printf '%s' "${text/"$from"/"$to"}" > "$file"
}
# Applies every edit of a mutant under the folder ROOT.
apply_all() {
	local root=$1 e f rest r
	local -a list
	IFS=$RS read -r -d '' -a list < <(printf '%s\0' "$2")
	for e in "${list[@]}"; do
		f=${e%%"$US"*} rest=${e#*"$US"}
		r=$(apply "$root/$f" "${rest%%"$US"*}" "${rest#*"$US"}") || { echo "$f: $r"; return 1; }
	done
}

if [ "$check_only" = 1 ]; then
	bad=0
	for i in "${!names[@]}"; do
		picked "${names[$i]}" || continue
		t=$(mktemp -d)
		(cd "$REPO" && git ls-files -co --exclude-standard -z | xargs -0 cp --parents -t "$t")
		if r=$(apply_all "$t" "${edits[$i]}"); then echo "applies      ${names[$i]}"
		else echo "NOT APPLIED  ${names[$i]} ($r)"; bad=1; fi
		rm -rf "$t"
	done
	exit "$bad"
fi
results=$OUT/results
: > "$results"
fast=() slow=()
for i in "${!names[@]}"; do
	picked "${names[$i]}" || continue
	if [ "${suites[$i]}" = sandbox ]; then slow+=("$i"); else fast+=("$i"); fi
done
echo "tests/mutate.sh: ${#fast[@]} mutants $JOBS at a time, then ${#slow[@]} in the sandbox, one at a time"
running=0
for i in "${fast[@]}"; do
	one "$i" | tee -a "$results" &
	running=$((running + 1))
	if [ "$running" -ge "$JOBS" ]; then wait -n; running=$((running - 1)); fi
done
wait
for i in "${slow[@]}"; do one "$i" | tee -a "$results"; done
total=$(wc -l < "$results")
caught=$(grep -c '^caught' "$results")
echo "mutate: $caught of $total caught"
[ "$caught" = "$total" ]
