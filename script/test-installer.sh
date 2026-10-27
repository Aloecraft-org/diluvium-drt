#!/bin/sh
# Tests for install.sh, offline: a fake mirror and release log under a
# temporary directory, reached through DRT_MIRROR and DRT_RELEASE_LOG as
# file:// URLs. The binaries are shell scripts that print a version, so this
# runs on Linux and macOS with no network.
#
#   script/test-installer.sh            the checkout's install.sh, under sh
#   SH=dash script/test-installer.sh    under another shell
#
# CI runs it under dash and bash (.github/workflows/ci.yml, installer).
set -eu
cd "$(dirname "$0")/.."

SH="${SH:-sh}"
SCRIPT="$PWD/install.sh"
NAME=drt
ASSETS="drt_linux_x86_64_musl drt_linux_arm64_musl drt_darwin_x86_64 drt_darwin_arm64"

# ---------------------------------------------------------------- depth: the fixture
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
M="$T/mirror"
mkdir -p "$M"

release() {  # $1 = tag; writes fake binaries and their sums
    mkdir -p "$M/$1"
    for a in $ASSETS; do
        printf '#!/bin/sh\n[ "${1:-}" != completions ] || { echo "# %s completion for $2"; exit; }\necho "%s %s"\n' "$NAME" "$NAME" "${1#v}" > "$M/$1/$a"
        chmod +x "$M/$1/$a"
    done
    (cd "$M/$1" && sha256sum -- * > SHA256SUMS.txt 2>/dev/null || shasum -a 256 -- * > SHA256SUMS.txt)
}
release v1.0.0
release v1.1.0
release v1.2.0-dev.1
release v1.0.1
# v1.0.1's sums name other bytes: the install must refuse it.
for a in $ASSETS; do echo "tampered" >> "$M/v1.0.1/$a"; done

cat > "$M/releases.json" <<'EOF'
{
  "latest": "v1.1.0",
  "releases": [
    {
      "tag": "v1.1.0",
      "stable": true
    },
    {
      "tag": "v1.0.0",
      "stable": true
    }
  ]
}
EOF
printf '# test log\nv1.2.0-dev.1\tdev\t2026-01-03\nv1.1.0\tstable\t2026-01-02\nv1.1.0-rc.1\trc\t2026-01-02\nv1.0.0\tstable\t2026-01-01\n' > "$T/release-log"

export DRT_MIRROR="file://$M" DRT_RELEASE_LOG="file://$T/release-log"
unset DRT_VERSION DRT_PREFIX DRT_SLIM 2>/dev/null || true
P="$T/prefix"

stamp() {  # $1 = mode, $2 = tag; prints the path of a stamped copy
    out="$T/$1-$2.sh"
    sed -e "s|^STAMPED_VERSION=.*|STAMPED_VERSION=$2|" -e "s|^STAMPED_MODE=.*|STAMPED_MODE=$1|" "$SCRIPT" > "$out"
    printf '%s\n' "$out"
}

# ---------------------------------------------------------------- depth: the cases
fails=0
ok()   { printf 'ok   %s\n' "$1"; }
bad()  { printf 'FAIL %s\n' "$1"; fails=$((fails + 1)); }
same() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1"; printf '  want: %s\n  got:  %s\n' "$3" "$2"; fi; }
has()  { case "$2" in *"$3"*) ok "$1" ;; *) bad "$1"; printf '  want it to contain: %s\n  got: %s\n' "$3" "$2" ;; esac; }
fails_with() {  # $1 = name, $2 = expected text, rest = command
    n="$1"; want="$2"; shift 2
    if out=$("$@" 2>&1); then bad "$n (it succeeded)"; else has "$n" "$out" "$want"; fi
}
in_use() { readlink "$P/bin/$NAME" | sed "s|.*/versions/\([^/]*\)/.*|\1|"; }

same "checkout copy --version" "$($SH "$SCRIPT" --version)" "none (a checkout copy, which installs latest)"
same "stamped --version" "$($SH "$(stamp install v1.0.0)" --version)" "v1.0.0"
same "stamped -r" "$($SH "$(stamp get v1.0.0)" -r)" "v1.0.0"
has  "help mode prints usage" "$($SH "$(stamp help v1.0.0)")" "list-all"
has  "--help in any mode" "$($SH "$SCRIPT" --help)" "remove [VERSION]"
has  "piped, through sh -s --" "$($SH -s -- --help < "$SCRIPT")" "usage:"
fails_with "unknown option refuses" "unknown option" $SH "$SCRIPT" --bogus

same "list: stable only, newest first" "$($SH "$SCRIPT" list | tr '\n' ' ')" "v1.1.0 v1.0.0 "
has  "list-all: dev builds too" "$($SH "$SCRIPT" list-all)" "v1.2.0-dev.1"

$SH "$SCRIPT" --prefix "$P" >/dev/null
same "start installs latest" "$(in_use)" "v1.1.0"
same "the link runs" "$("$P/bin/$NAME" --version)" "$NAME 1.1.0"
completions() {  # every completion file an install writes
    printf '%s\n' "$P/share/bash-completion/completions/$NAME" "$P/share/zsh/site-functions/_$NAME" "$P/share/fish/vendor_completions.d/$NAME.fish"
}
missing=""; for f in $(completions); do [ -s "$f" ] || missing="$missing $f"; done
same "tab completion for bash, zsh and fish, from the binary" "$missing" ""
same "each from its own shell" "$(cat "$P/share/zsh/site-functions/_$NAME")" "# $NAME completion for zsh"

$SH "$(stamp install v1.0.0)" --prefix "$P" >/dev/null
same "install.sh installs its own release" "$(in_use)" "v1.0.0"

$SH "$(stamp get v1.0.0)" install latest --prefix "$P" >/dev/null
same "get.sh install latest behaves as install" "$(in_use)" "v1.1.0"

$SH "$SCRIPT" --prefix "$P" -r 1.2.0-dev.1 >/dev/null
same "-r without the v" "$(in_use)" "v1.2.0-dev.1"
$SH "$SCRIPT" --prefix "$P" use 1.0.0 >/dev/null
$SH "$SCRIPT" --prefix "$P" install dev >/dev/null
same "dev resolves through the release log" "$(in_use)" "v1.2.0-dev.1"

check=$($SH "$SCRIPT" --prefix "$P" check)
has "check lists every release" "$check" "v1.0.0"
has "check marks the one in use" "$check" "v1.2.0-dev.1  $P  (in use)"

$SH "$SCRIPT" --prefix "$P" use v1.0.0 >/dev/null
same "use switches" "$("$P/bin/$NAME" --version)" "$NAME 1.0.0"
fails_with "use refuses what is not installed" "not installed" $SH "$SCRIPT" --prefix "$P" use v9.9.9

fails_with "a checksum mismatch refuses" "checksum mismatch" $SH "$SCRIPT" --prefix "$P" -r v1.0.1
same "a refused install changes nothing" "$(in_use)" "v1.0.0"

$SH "$SCRIPT" --prefix "$P" remove v1.0.0 >/dev/null
[ ! -e "$P/bin/$NAME" ] && ok "removing the one in use unlinks it" || bad "removing the one in use unlinks it"
has "the others stay" "$($SH "$SCRIPT" --prefix "$P" check)" "v1.1.0"

$SH "$SCRIPT" --prefix "$P" remove >/dev/null
[ ! -d "$P/share/$NAME" ] && ok "remove with no version removes all" || bad "remove with no version removes all"
left=""; for f in $(completions); do [ ! -e "$f" ] || left="$left $f"; done
same "the completions with it" "$left" ""

mkdir "$T/here"
(cd "$T/here" && $SH "$(stamp get v1.0.0)" >/dev/null)
same "get.sh saves its release here" "$("$T/here/$NAME" --version)" "$NAME 1.0.0"

if [ "$fails" -eq 0 ]; then echo "PASS ($SH)"; else echo "$fails failed ($SH)"; exit 1; fi
