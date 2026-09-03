#!/usr/bin/env bash
#
# run-all.sh — run every numbered example and diff its output against the
# expected.txt beside it.  This is the gate that keeps the example set from
# rotting: an example whose output has drifted from its README fails here.
#
# Dependencies: bash, and coreutils + sed + diff.  Nothing else — in
# particular no jq, so meta.json is read by the small JSON reader below.

set -o pipefail

SELF=${0##*/}
HERE=$(cd -- "$(dirname -- "$0")" && pwd)

usage() {
    cat <<EOF
$SELF — run every example in $HERE and diff it against its expected.txt.

usage: $SELF [options] [example ...]

Each examples/NN-*/ directory that contains a meta.json is one example.
For each, $SELF changes into that directory, runs meta.json's "cmd" with
stdout and stderr merged, applies meta.json's "normalise" sed expressions
to both that output and expected.txt, and diffs the two.  It prints one
ok/FAILED line per example, the diff for each failure, and a summary.

It exits non-zero if any example that RAN failed.  An example skipped for
needing a network is never counted as a pass — it is printed as
"skipped (needs network)" and named again in the summary.

options:
  --net        Also run examples whose meta.json sets "needs_network": true.
               Without this they are skipped, loudly.
  --list       List the examples that would run, and exit.
  --keep       Do not restore each example directory after its run.  Use
               this to inspect files an example wrote; without it, every
               directory is put back the way it was found.
  -h, --help   This text.

arguments:
  Any number of example names.  A name matches if it is a prefix of, or a
  substring of, the directory name — so "04", "04-files" and "files" all
  select examples/04-files.  With no names, every example runs.

environment:
  DRT          The drt binary to test.  Defaults to "drt" from PATH.  It is
               resolved to an absolute path and put on PATH under the name
               "drt", so a meta.json "cmd" always spells the command the
               README spells, and CI can point this at a build.

               Build it with --all-features.  A bare `cargo build` is a
               SLIM binary -- no sql, ssh, rest, netcheck, tunnel or relay
               -- and eight of these examples need those, so they are
               skipped rather than run and diffed:

                   cargo build --release --all-features
                   DRT=../target/release/drt ./$SELF

  TIMEOUT      Seconds any one example may run before it is killed and
               reported as timed out.  Default 120; 0 disables.  Needs
               coreutils' timeout(1); without it the runs are unbounded.

  LC_ALL is pinned to C for the runs, so sed, diff and any sorted output
  are collation-stable.  Nothing else about the environment is changed.

An NN-* directory with no meta.json is reported as "NO META" and fails the
run.  It is not skipped quietly: a directory the gate cannot check does not
get to sit inside a set that reports itself as passing.  Give it a meta.json.

meta.json:
  {
    "name":          "04-files",          # informational
    "teaches":       "...",               # informational
    "cmd":           "drt run app.dlua",  # run by bash, stdout+stderr captured
    "needs_network": false,               # true => skipped unless --net
    "needs_build":   "full",              # skipped unless `drt buildinfo`
                                          # reports that profile.  `cargo
                                          # build` with no flags is SLIM.
    "normalise":     ["s|a|b|"]           # sed -e, applied to BOTH sides
  }

  "normalise" exists for output that cannot be byte-stable (a clock
  reading, a line number).  Each expression is applied to the actual output
  and to expected.txt, in order, so a well-written one is idempotent
  against an expected.txt that already carries its placeholder.
EOF
}

# ---------------------------------------------------------------------------
# A JSON reader, for exactly the shape a meta.json has: one flat object whose
# values are strings, booleans, numbers, or arrays of strings.  Values of any
# other shape are skipped rather than rejected, so an unknown key with a
# nested value does not break the run.
# ---------------------------------------------------------------------------

_j_s=""     # document
_j_i=0      # cursor
_j_n=0      # length
j_err=""

# Does this bash's printf understand \uXXXX?  Decided once, so a meta.json
# that uses one gets an error rather than a mangled expression.
if [ "$(printf '\u0041' 2>/dev/null)" = "A" ]; then _j_u=1; else _j_u=0; fi

j_fail() { j_err=$1; return 1; }

j_ws() {
    while [ "$_j_i" -lt "$_j_n" ]; do
        case ${_j_s:$_j_i:1} in
            ' '|$'\t'|$'\r'|$'\n') _j_i=$((_j_i + 1)) ;;
            *) return 0 ;;
        esac
    done
}

# Reads a quoted string at the cursor.  Result in $REPLY.
j_string() {
    REPLY=""
    [ "${_j_s:$_j_i:1}" = '"' ] || j_fail "expected a string at byte $_j_i" || return 1
    _j_i=$((_j_i + 1))
    local c u
    while [ "$_j_i" -lt "$_j_n" ]; do
        c=${_j_s:$_j_i:1}; _j_i=$((_j_i + 1))
        case $c in
            '"') return 0 ;;
            '\')
                c=${_j_s:$_j_i:1}; _j_i=$((_j_i + 1))
                case $c in
                    n)  REPLY=$REPLY$'\n' ;;
                    t)  REPLY=$REPLY$'\t' ;;
                    r)  REPLY=$REPLY$'\r' ;;
                    b)  REPLY=$REPLY$'\b' ;;
                    f)  REPLY=$REPLY$'\f' ;;
                    '/'|'\'|'"') REPLY=$REPLY$c ;;
                    u)  u=${_j_s:$_j_i:4}; _j_i=$((_j_i + 4))
                        [ "$_j_u" = 1 ] || j_fail "\\u$u needs a newer bash" || return 1
                        REPLY=$REPLY$(printf "\\u$u") ;;
                    *)  j_fail "bad escape \\$c at byte $_j_i" || return 1 ;;
                esac ;;
            *) REPLY=$REPLY$c ;;
        esac
    done
    j_fail "unterminated string"
}

# Reads a bare literal (true / false / null / a number).  Result in $REPLY.
j_literal() {
    REPLY=""
    local c
    while [ "$_j_i" -lt "$_j_n" ]; do
        c=${_j_s:$_j_i:1}
        case $c in
            ','|'}'|']'|' '|$'\t'|$'\r'|$'\n'|'') break ;;
            *) REPLY=$REPLY$c; _j_i=$((_j_i + 1)) ;;
        esac
    done
    [ -n "$REPLY" ] || j_fail "expected a value at byte $_j_i"
}

# Consumes one value of any shape, discarding it.
j_skip() {
    local depth=0 c
    j_ws
    case ${_j_s:$_j_i:1} in
        '"') j_string; return $? ;;
        '['|'{') ;;
        *) j_literal; return $? ;;
    esac
    while [ "$_j_i" -lt "$_j_n" ]; do
        c=${_j_s:$_j_i:1}
        case $c in
            '"') j_string || return 1; continue ;;
            '['|'{') depth=$((depth + 1)) ;;
            ']'|'}') depth=$((depth - 1)) ;;
        esac
        _j_i=$((_j_i + 1))
        [ "$depth" -gt 0 ] || return 0
    done
    j_fail "unterminated array or object"
}

# ---------------------------------------------------------------------------
# meta.json -> meta_cmd, meta_net, meta_name, meta_norm[]
# ---------------------------------------------------------------------------

parse_meta() {
    local file=$1 key
    meta_cmd=""; meta_net="false"; meta_name=""; meta_norm=(); j_err=""
    meta_build=""

    _j_s=$(cat -- "$file") || { j_err="cannot read it"; return 1; }
    _j_n=${#_j_s}; _j_i=0

    j_ws
    [ "${_j_s:$_j_i:1}" = '{' ] || { j_err="top level is not a JSON object"; return 1; }
    _j_i=$((_j_i + 1))
    j_ws
    if [ "${_j_s:$_j_i:1}" = '}' ]; then return 0; fi

    while :; do
        j_ws
        j_string || return 1
        key=$REPLY
        j_ws
        [ "${_j_s:$_j_i:1}" = ':' ] || { j_err="expected ':' after \"$key\""; return 1; }
        _j_i=$((_j_i + 1))
        j_ws
        case $key in
            cmd)           j_string  || return 1; meta_cmd=$REPLY ;;
            name)          j_string  || return 1; meta_name=$REPLY ;;
            needs_network) j_literal || return 1; meta_net=$REPLY ;;
            needs_build)   j_string  || return 1; meta_build=$REPLY ;;
            normalise|normalize)
                [ "${_j_s:$_j_i:1}" = '[' ] || { j_err="\"$key\" is not an array"; return 1; }
                _j_i=$((_j_i + 1))
                j_ws
                if [ "${_j_s:$_j_i:1}" = ']' ]; then
                    _j_i=$((_j_i + 1))
                else
                    while :; do
                        j_ws
                        j_string || return 1
                        meta_norm[${#meta_norm[@]}]=$REPLY
                        j_ws
                        case ${_j_s:$_j_i:1} in
                            ',') _j_i=$((_j_i + 1)) ;;
                            ']') _j_i=$((_j_i + 1)); break ;;
                            *) j_err="expected ',' or ']' in \"$key\""; return 1 ;;
                        esac
                    done
                fi ;;
            *) j_skip || return 1 ;;
        esac
        j_ws
        case ${_j_s:$_j_i:1} in
            ',') _j_i=$((_j_i + 1)) ;;
            '}') return 0 ;;
            *) j_err="expected ',' or '}' after \"$key\""; return 1 ;;
        esac
    done
}

# ---------------------------------------------------------------------------
# Snapshot / restore, so a run leaves the tree the way it found it.
# ---------------------------------------------------------------------------

snapshot() {   # snapshot <dir> <into>
    rm -rf -- "$2" && mkdir -p -- "$2" && cp -a -- "$1/." "$2/"
}

restore() {    # restore <dir> <from-snapshot>
    local p
    # Anything the run created that was not there before.
    while IFS= read -r p; do
        [ -e "$2/$p" ] || [ -L "$2/$p" ] || rm -rf -- "$1/$p"
    done < <(cd -- "$1" && find . -mindepth 1 2>/dev/null)
    # Anything the run changed or deleted.
    while IFS= read -r p; do
        cmp -s -- "$2/$p" "$1/$p" 2>/dev/null && continue
        rm -rf -- "$1/$p"
        mkdir -p -- "$(dirname -- "$1/$p")"
        cp -a -- "$2/$p" "$1/$p"
    done < <(cd -- "$2" && find . -mindepth 1 -type f 2>/dev/null)
}

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------

want_net=0
do_list=0
keep=0
selectors=()

while [ $# -gt 0 ]; do
    case $1 in
        --net)      want_net=1 ;;
        --list)     do_list=1 ;;
        --keep)     keep=1 ;;
        -h|--help)  usage; exit 0 ;;
        --)         shift; while [ $# -gt 0 ]; do selectors[${#selectors[@]}]=$1; shift; done; break ;;
        -*)         printf '%s: unknown option %s (try --help)\n' "$SELF" "$1" >&2; exit 2 ;;
        *)          selectors[${#selectors[@]}]=$1 ;;
    esac
    shift
done

selected() {   # selected <dirname>
    [ ${#selectors[@]} -eq 0 ] && return 0
    local s
    for s in "${selectors[@]}"; do
        case $1 in *"$s"*) return 0 ;; esac
    done
    return 1
}

# ---------------------------------------------------------------------------
# Collect the examples
# ---------------------------------------------------------------------------

examples=()
uncovered=()
for d in "$HERE"/[0-9][0-9]-*/; do
    [ -d "$d" ] || continue
    n=${d%/}; n=${n##*/}
    selected "$n" || continue
    if [ -f "$d/meta.json" ]; then
        examples[${#examples[@]}]=$n
    else
        uncovered[${#uncovered[@]}]=$n
    fi
done

if [ ${#examples[@]} -eq 0 ] && [ ${#uncovered[@]} -eq 0 ]; then
    if [ ${#selectors[@]} -gt 0 ]; then
        printf '%s: no example in %s matches: %s\n' "$SELF" "$HERE" "${selectors[*]}" >&2
    else
        printf '%s: found no NN-*/meta.json under %s\n' "$SELF" "$HERE" >&2
    fi
    exit 2
fi

if [ "$do_list" = 1 ]; then
    for n in ${examples[@]+"${examples[@]}"}; do printf '%s\n' "$n"; done
    for n in ${uncovered[@]+"${uncovered[@]}"}; do printf '%s   (no meta.json)\n' "$n"; done
    exit 0
fi

# ---------------------------------------------------------------------------
# Resolve the binary under test, and put it on PATH as "drt".
#
# meta.json spells the command the README spells — "drt run app.dlua" — and
# rewriting that text would also rewrite the "$ drt run app.dlua" banners the
# examples echo into their own expected output.  So instead of editing the
# command, we edit PATH.
# ---------------------------------------------------------------------------

DRT=${DRT:-drt}
case $DRT in
    */*) drt_abs=$(cd -- "$(dirname -- "$DRT")" 2>/dev/null && pwd)/$(basename -- "$DRT") ;;
    *)   drt_abs=$(command -v -- "$DRT" 2>/dev/null) ;;
esac

if [ -z "$drt_abs" ] || [ ! -x "$drt_abs" ]; then
    printf '%s: DRT=%s is not an executable I can find.\n' "$SELF" "$DRT" >&2
    printf '    Put drt on PATH, or point DRT at a build:\n' >&2
    printf '        DRT=../target/debug/drt %s\n' "$SELF" >&2
    exit 2
fi

# The demo.sh scripts that honour $DRT get the absolute path too. The
# README's `DRT=../target/release/drt` is relative to examples/, and an
# example runs from inside its own directory, where that path names
# nothing -- so 13 and 15 failed under the documented command while the
# fourteen that only use PATH passed.
DRT=$drt_abs
export DRT

work=$(mktemp -d) || exit 2
trap 'rm -rf -- "$work"' EXIT INT TERM

mkdir -p "$work/bin"
printf '#!/bin/sh\nexec %s "$@"\n' "$(printf '%q' "$drt_abs")" > "$work/bin/drt"
chmod +x "$work/bin/drt"
PATH=$work/bin:$PATH
export PATH
export LC_ALL=C

# A hung example is rot too: it takes CI down with no line naming the culprit.
# timeout(1) is coreutils, but it is not on every box, so this degrades to an
# unbounded run rather than refusing to work.
TIMEOUT=${TIMEOUT:-120}
case $TIMEOUT in
    ''|*[!0-9]*) printf '%s: TIMEOUT=%s is not a whole number of seconds\n' \
                     "$SELF" "$TIMEOUT" >&2; exit 2 ;;
esac
limit=()
if [ "$TIMEOUT" != 0 ] && command -v timeout >/dev/null 2>&1; then
    limit=(timeout -k 5 "$TIMEOUT")
fi

printf 'drt: %s\n' "$drt_abs"
"$drt_abs" buildinfo 2>/dev/null | sed -n 's/^\(version\|profile\): /     \1: /p'

# Which profile this binary is, for the needs_build gate below. `unknown`
# when buildinfo cannot be read at all, and an unknown profile runs
# everything rather than skipping everything -- a gate that silently stops
# checking is worse than one that reports a diff.
drt_profile=$("$drt_abs" buildinfo 2>/dev/null | sed -n 's/^profile: //p')
drt_profile=${drt_profile:-unknown}
if [ "$drt_profile" != full ] && [ "$drt_profile" != unknown ]; then
    printf '\n%s: this is a %s build, so examples needing the full connector\n' "$SELF" "$drt_profile"
    printf '%s  set are skipped.  For the whole set:\n\n' "${SELF//?/ }"
    printf '    cargo build --release --all-features\n'
    printf '    DRT=../target/release/drt ./%s\n' "$SELF"
fi
printf '\n'

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

MAXDIFF=200
n_ok=0
n_fail=0
failed=()
skipped=()
wrong_build=()

for name in ${examples[@]+"${examples[@]}"}; do
    dir=$HERE/$name

    if ! parse_meta "$dir/meta.json"; then
        printf 'FAILED   %-24s meta.json: %s\n' "$name" "$j_err"
        n_fail=$((n_fail + 1)); failed[${#failed[@]}]=$name
        continue
    fi

    if [ -z "$meta_cmd" ]; then
        printf 'FAILED   %-24s meta.json has no "cmd"\n' "$name"
        n_fail=$((n_fail + 1)); failed[${#failed[@]}]=$name
        continue
    fi

    if [ "$meta_net" = "true" ] && [ "$want_net" != 1 ]; then
        printf 'skipped  %-24s (needs network) — pass --net to run it\n' "$name"
        skipped[${#skipped[@]}]=$name
        continue
    fi

    # An example that needs connectors or verbs this binary does not carry
    # cannot be run, and running it anyway produces a diff that reads like a
    # regression.  `cargo build` with no flags is a SLIM build, and slim is
    # missing sql, ssh, rest, netcheck, tunnel and relay -- so the obvious
    # invocation used to fail eight examples at once, each with a diff whose
    # real content was "this build does not carry that".
    #
    # Skipped, named, and never a pass.  Same rule as the network skip.
    if [ -n "$meta_build" ] && [ "$meta_build" != "$drt_profile" ] \
       && [ "$drt_profile" != unknown ]; then
        printf 'skipped  %-24s (needs a %s build; this drt is %s)\n' \
            "$name" "$meta_build" "$drt_profile"
        wrong_build[${#wrong_build[@]}]=$name
        continue
    fi

    if [ ! -f "$dir/expected.txt" ]; then
        printf 'FAILED   %-24s no expected.txt beside meta.json\n' "$name"
        n_fail=$((n_fail + 1)); failed[${#failed[@]}]=$name
        continue
    fi

    snap=$work/snap.$name
    have_snap=0
    if [ "$keep" != 1 ] && snapshot "$dir" "$snap"; then have_snap=1; fi

    ( cd -- "$dir" && exec ${limit[@]+"${limit[@]}"} bash -c "$meta_cmd" ) \
        > "$work/actual.raw" 2>&1
    status=$?

    [ "$have_snap" = 1 ] && restore "$dir" "$snap"

    if [ ${#limit[@]} -gt 0 ] && { [ "$status" = 124 ] || [ "$status" = 137 ]; }; then
        printf 'FAILED   %-24s timed out after %ss (set TIMEOUT= to change)\n' \
            "$name" "$TIMEOUT"
        n_fail=$((n_fail + 1)); failed[${#failed[@]}]=$name
        continue
    fi

    if [ ${#meta_norm[@]} -gt 0 ]; then
        seds=()
        for e in "${meta_norm[@]}"; do seds[${#seds[@]}]="-e"; seds[${#seds[@]}]=$e; done
        if ! sed "${seds[@]}" "$work/actual.raw" > "$work/actual.txt" 2>"$work/sed.err" ||
           ! sed "${seds[@]}" "$dir/expected.txt" > "$work/expect.txt" 2>>"$work/sed.err"; then
            printf 'FAILED   %-24s meta.json "normalise" is not valid sed:\n' "$name"
            sed 's/^/           /' "$work/sed.err"
            n_fail=$((n_fail + 1)); failed[${#failed[@]}]=$name
            continue
        fi
    else
        cp -- "$work/actual.raw" "$work/actual.txt"
        cp -- "$dir/expected.txt" "$work/expect.txt"
    fi

    if diff -u "$work/expect.txt" "$work/actual.txt" > "$work/diff.txt" 2>&1; then
        if [ "$status" = 0 ]; then
            printf 'ok       %-24s %s\n' "$name" "$meta_cmd"
        else
            printf 'ok       %-24s %s   [exit %d]\n' "$name" "$meta_cmd" "$status"
        fi
        n_ok=$((n_ok + 1))
    else
        printf 'FAILED   %-24s %s   [exit %d]\n' "$name" "$meta_cmd" "$status"
        printf '           --- expected.txt      +++ actual\n'
        sed -e '1,2d' -e 's/^/           /' "$work/diff.txt" | sed -n "1,${MAXDIFF}p"
        lines=$(sed -e '1,2d' "$work/diff.txt" | wc -l)
        if [ "$lines" -gt "$MAXDIFF" ]; then
            printf '           ... %d more diff lines not shown\n' "$((lines - MAXDIFF))"
        fi
        n_fail=$((n_fail + 1)); failed[${#failed[@]}]=$name
    fi
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

n_skip=$((${#skipped[@]} + ${#wrong_build[@]}))
n_bare=${#uncovered[@]}

for n in ${uncovered[@]+"${uncovered[@]}"}; do
    printf 'NO META  %-24s not checked by anything — add a meta.json\n' "$n"
done

total=$((n_ok + n_fail + n_skip + n_bare))

printf '\n'
printf '%d example(s): %d ok, %d failed, %d skipped, %d without a meta.json\n' \
    "$total" "$n_ok" "$n_fail" "$n_skip" "$n_bare"

# Each skip reason names itself. A skip is never a pass, and a summary that
# says only "1 skipped" makes the reader go and find out which and why.
if [ ${#skipped[@]} -gt 0 ]; then
    printf 'skipped for needing a network (NOT a pass): %s\n' "${skipped[*]}"
    printf 'run with --net to include them.\n'
fi
if [ ${#wrong_build[@]} -gt 0 ]; then
    printf 'skipped for needing a full build (NOT a pass): %s\n' "${wrong_build[*]}"
    printf 'rebuild with --all-features to include them.\n'
fi
if [ "$n_bare" -gt 0 ]; then
    printf 'no meta.json, so unchecked: %s\n' "${uncovered[*]}"
fi
if [ "$n_fail" -gt 0 ]; then
    printf 'failed: %s\n' "${failed[*]}"
fi
if [ "$n_fail" -gt 0 ] || [ "$n_bare" -gt 0 ]; then
    exit 1
fi
exit 0
