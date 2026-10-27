#!/bin/sh
# DRT, the Diluvium RunTime: start, install and get, in one script.
#
#   curl -fsSL https://diluvium.aloecraft.org/start-drt | sh
#   curl -fsSL https://software.aloecraft.org/releases/diluvium-drt/latest/install.sh | sh
#
# Every release ships this file six times, as start.sh, start-drt.sh,
# install.sh, install-drt.sh, get.sh and get-drt.sh. release.yml stamps two
# lines in each copy: the release that shipped it, and its mode. A script
# piped to `sh` cannot see the URL it came from, so the stamp is the only
# way a copy can know what it is. The checkout's copy is an unstamped start
# script.
#
# POSIX sh: no arrays, no `local`, no bashisms. It runs on busybox ash and on
# macOS's /bin/sh. curl or wget, and sha256sum, shasum or openssl.
set -eu

# ---------------------------------------------------------------- surface
# Entry points. With no command, MODE decides:
#   start    install the latest stable release
#   install  install the release that shipped this copy
#   get      download and verify that release into the current directory
usage() {
    cat <<EOF
usage: $SELF [command] [--version VERSION] [--prefix DIR] [--slim]

  (none)            $(mode_says)
  install [VERSION] install VERSION, or the default above, and use it
  get [VERSION]     download and verify VERSION into the current directory
  latest            as VERSION: the newest stable release
  dev               as VERSION: the newest dev build
  --version, -r, --release [VERSION]
                    with VERSION, the release to act on; alone, print the
                    release this copy came from
  list              stable releases available
  list-all          every release, candidates and dev builds included
  check             what is installed, and which one is in use
  use VERSION       switch to an installed release
  remove [VERSION]  uninstall VERSION, or every release

  --prefix DIR      install under DIR/bin and DIR/share/drt
                    (default /usr/local if writable, else ~/.local)
  --slim            the slim profile

Env: DRT_VERSION, DRT_PREFIX, DRT_SLIM=1, DRT_MIRROR (a file:// URL works).
EOF
}

# Stamped by release.yml. Empty means this copy came from no release.
STAMPED_VERSION=
STAMPED_MODE=start

# Configurable values.
NAME=drt
REPO=Aloecraft-org/diluvium-drt
MIRROR="${DRT_MIRROR:-https://software.aloecraft.org/releases/diluvium-drt}"
GITHUB="https://github.com/$REPO"
RAW="https://raw.githubusercontent.com/$REPO"
# Every published release, dev builds included, one per line: tag, channel
# (stable, rc or dev), date. release.yml rewrites it on its own branch on
# every publish, so it lists what GitHub can serve and nothing pruned.
RELEASE_LOG="$RAW/release-log/.github/release-log"
SELFTEST=--version
PREFIX="${DRT_PREFIX:-}"
SLIM="${DRT_SLIM:-0}"

# Fan-out: the command picks the action.
dispatch() {
    case "$ACTION" in
        print)    say "${STAMPED_VERSION:-none (a checkout copy, which installs latest)}" ;;
        install)  do_install ;;
        get)      do_get ;;
        list)     do_list stable ;;
        list-all) do_list all ;;
        check)    do_check ;;
        use)      do_use ;;
        remove)   do_remove ;;
    esac
}

# The files a release carries for this platform, one per line:
# <installed name> <asset> [<the asset's older name>]. The first line is
# required; any after it are optional extras.
platform() {
    case "$(uname -s)" in
        Linux)  os=linux ;;
        Darwin) os=darwin ;;
        MINGW*|MSYS*|CYGWIN*)
            die "on Windows, take drt_windows_x86_64.exe (or _slim) from $MIRROR/latest/ and verify it against SHA256SUMS.txt there" ;;
        *) die "$(uname -s) has no prebuilt DRT; build it with 'cargo build --release --features full -p drt'" ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64)  arch=x86_64 ;;
        arm64|aarch64) arch=arm64 ;;
        *) die "$(uname -m) has no prebuilt DRT yet" ;;
    esac
    # doc/ALIGNMENT.md §4: <os>_<arch>[_<libc>], the profile last. Releases
    # before v0.6.0-rc.2 spelled Linux `linux_static` and put the profile
    # first, so a pinned older tag has only the old name.
    libc=""; old_os="$os"
    if [ "$os" = linux ]; then libc=_musl; old_os=linux_static; fi
    if [ "$SLIM" = 1 ]; then
        WANT="drt drt_${os}_${arch}${libc}_slim drt_slim_${old_os}_${arch}"
    else
        WANT="drt drt_${os}_${arch}${libc} drt_${old_os}_${arch}"
    fi
}

# ---------------------------------------------------------------- arguments
SELF=$(basename "$0" 2>/dev/null || echo "$NAME")
case "$SELF" in sh|bash|dash|ash|-*) SELF="$STAMPED_MODE.sh" ;; esac

say() { printf '%s\n' "$*"; }
die() { printf '%s: %s\n' "$SELF" "$*" >&2; exit 1; }

mode_says() {
    case "$STAMPED_MODE" in
        start)   echo "install the latest stable release" ;;
        install) echo "install ${STAMPED_VERSION:-latest}, the release this copy came from" ;;
        get)     echo "get ${STAMPED_VERSION:-latest}, the release this copy came from" ;;
    esac
}

# A word after --version is its value unless it is an option or a command.
is_value() {
    case "$1" in
        -*|install|get|list|list-all|check|use|remove) return 1 ;;
    esac
    return 0
}

ACTION=""; TARGET=""; PRINT=0
while [ $# -gt 0 ]; do
    case "$1" in
        install|get|list|list-all|check|use|remove)
            [ -z "$ACTION" ] || die "one command at a time: $ACTION, then $1"
            ACTION="$1" ;;
        --version|-r|--release)
            if [ $# -gt 1 ] && is_value "$2"; then TARGET="$2"; shift; else PRINT=1; fi ;;
        --version=*|--release=*) TARGET="${1#*=}" ;;
        --prefix)   [ $# -gt 1 ] || die "--prefix needs a directory"; PREFIX="$2"; shift ;;
        --prefix=*) PREFIX="${1#*=}" ;;
        --slim)     SLIM=1 ;;
        -h|--help)  usage; exit 0 ;;
        -*)         die "unknown option: $1 (try --help)" ;;
        *)
            [ -z "$TARGET" ] || die "two versions given: $TARGET and $1"
            TARGET="$1" ;;
    esac
    shift
done

if [ -z "$ACTION" ]; then
    if [ "$PRINT" = 1 ] && [ -z "$TARGET" ]; then
        ACTION=print
    else
        case "$STAMPED_MODE" in get) ACTION=get ;; *) ACTION=install ;; esac
    fi
fi
if [ -z "$TARGET" ]; then
    case "$STAMPED_MODE" in
        start) TARGET="${DRT_VERSION:-latest}" ;;
        *)     TARGET="${DRT_VERSION:-${STAMPED_VERSION:-latest}}" ;;
    esac
    # `remove` with no version means every version, not the default one.
    [ "$ACTION" != remove ] || TARGET=""
fi
[ "$ACTION" != use ] || [ -n "$TARGET" ] || die "use needs a version (see '$SELF check')"

# ---------------------------------------------------------------- depth: fetching and verifying
have() { command -v "$1" >/dev/null 2>&1; }

if have curl; then
    fetch()    { curl -fsSL --retry 2 -o "$2" "$1" 2>/dev/null; }
    fetchout() { curl -fsSL --retry 2 "$1" 2>/dev/null; }
elif have wget; then
    fetch()    { wget -q -O "$2" "$1"; }
    fetchout() { wget -q -O - "$1"; }
else
    die "neither curl nor wget is available"
fi

sha256_of() {
    if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
    elif have shasum;  then shasum -a 256 "$1" | cut -d' ' -f1
    elif have openssl; then openssl dgst -sha256 "$1" | sed 's/.*= *//'
    fi
}

json_latest() {
    sed -n 's/^[[:space:]]*"latest"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1
}

# Releases as "tag<TAB>channel", newest first, from a releases.json or a
# changelog.json. Both are pretty-printed with one key per line, and each
# release names its tag before its status and stability. An entry that is
# not released yet is left out.
json_releases() {
    awk '
        function emit() {
            if (tag != "" && stable != "" && status != "unreleased") {
                ch = (stable == "true") ? "stable" : "rc"
                if (tag ~ /-dev\./) ch = "dev"
                printf "%s\t%s\n", tag, ch
            }
            tag = ""; stable = ""; status = ""
        }
        /^[ \t]*"tag"[ \t]*:/    { emit(); v = $0; sub(/^[^:]*:[ \t]*"/, "", v); sub(/".*/, "", v); tag = v }
        /^[ \t]*"status"[ \t]*:/ { v = $0; sub(/^[^:]*:[ \t]*"/, "", v); sub(/".*/, "", v); status = v }
        /^[ \t]*"stable"[ \t]*:/ { v = $0; sub(/^[^:]*:[ \t]*/, "", v); sub(/[ \t,]*$/, "", v); stable = v }
        END { emit() }
    '
}

# The release log, without its comments: "tag<TAB>channel<TAB>date".
release_log() {
    fetchout "$RELEASE_LOG" | sed '/^#/d;/^[[:space:]]*$/d'
}

# Every source that can name the newest stable release, ours first.
resolve_latest() {
    tag=$(fetchout "$MIRROR/releases.json" | json_latest || true)
    [ -n "$tag" ] || tag=$(fetchout "$RAW/main/changelog.json" | json_latest || true)
    [ -n "$tag" ] || tag=$(release_log | awk -F'\t' '$2 == "stable" { print $1; exit }' || true)
    if [ -z "$tag" ] && have curl; then
        # GitHub answers /releases/latest with a redirect to the tag.
        tag=$(curl -fsSI "$GITHUB/releases/latest" 2>/dev/null \
            | sed -n 's|^[Ll]ocation:.*/releases/tag/\([^[:space:]]*\).*|\1|p' | head -1 || true)
    fi
    [ -n "$tag" ] || die "cannot tell which release is latest: the mirror, GitHub and the release log all failed"
    printf '%s\n' "$tag"
}

resolve() {
    case "$1" in
        latest) resolve_latest ;;
        dev)
            tag=$(release_log | awk -F'\t' '$2 == "dev" { print $1; exit }' || true)
            [ -n "$tag" ] || die "cannot find a dev build in $RELEASE_LOG"
            printf '%s\n' "$tag" ;;
        v*) printf '%s\n' "$1" ;;
        *)  printf 'v%s\n' "$1" ;;
    esac
}

# Download the release's files for this platform into $1, verified. Sets
# SOURCE to the base URL that served them. The mirror first, then GitHub
# Releases: a dev build is only ever on GitHub.
download() {
    dir="$1"
    platform
    first=$(printf '%s\n' "$WANT" | head -1)
    set -- $first
    want_name="$1"; shift
    SOURCE=""; got=""
    for base in "$MIRROR/$TAG" "$GITHUB/releases/download/$TAG"; do
        for asset in "$@"; do
            if fetch "$base/$asset" "$dir/$want_name"; then
                SOURCE="$base"; got="$asset"; break 2
            fi
        done
    done
    [ -n "$SOURCE" ] || die "no $* for $TAG at $MIRROR/$TAG or $GITHUB/releases/download/$TAG (see '$SELF list-all')"
    fetch "$SOURCE/SHA256SUMS.txt" "$dir/.sums" || : > "$dir/.sums"
    verify "$dir/$want_name" "$got"
    printf '%s\n' "$WANT" | sed 1d | while read -r extra asset _; do
        [ -n "$extra" ] || continue
        if fetch "$SOURCE/$asset" "$dir/$extra"; then
            verify "$dir/$extra" "$asset"
        else
            say "note: $TAG has no $asset"
        fi
    done
    rm -f "$dir/.sums"
}

# A missing sums file, or an asset it does not list, warns rather than
# refuses: that is a release older than the sums-publishing workflow, and
# refusing would strand whoever pinned it. A mismatch always refuses.
verify() {
    chmod +x "$1"
    want=$(grep " $2\$" "$(dirname "$1")/.sums" | cut -d' ' -f1 || true)
    if [ -z "$want" ]; then
        say "warning: $SOURCE/SHA256SUMS.txt does not list $2; not verified" >&2
        return 0
    fi
    have_sum=$(sha256_of "$1")
    [ -n "$have_sum" ] || { say "warning: no sha256 tool; $2 not verified" >&2; return 0; }
    [ "$want" = "$have_sum" ] || die "checksum mismatch for $2 from $SOURCE
  expected $want
  got      $have_sum"
}

# ---------------------------------------------------------------- depth: the install layout
# DIR/share/drt/versions/<tag>/drt holds each release; DIR/bin/drt is a link
# to the one in use. A plain file at DIR/bin/drt is an install from before
# this layout, and `use` replaces it.
default_prefix() {
    if [ -n "$PREFIX" ]; then return; fi
    if [ -w /usr/local/bin ] || [ "$(id -u)" = 0 ]; then PREFIX=/usr/local; else PREFIX="$HOME/.local"; fi
}

# For use, remove and check: the prefix that already holds installs.
installed_prefixes() {
    if [ -n "$PREFIX" ]; then printf '%s\n' "$PREFIX"; return; fi
    for p in /usr/local "$HOME/.local"; do
        [ -d "$p/share/$NAME/versions" ] && printf '%s\n' "$p"
    done
    return 0
}

active_in() {
    link=$(readlink "$1/bin/$NAME" 2>/dev/null || true)
    case "$link" in
        */share/$NAME/versions/*) printf '%s\n' "$link" | sed "s|.*/share/$NAME/versions/\([^/]*\)/.*|\1|" ;;
    esac
}

link_version() {
    vdir="$PREFIX/share/$NAME/versions/$TAG"
    mkdir -p "$PREFIX/bin"
    for f in "$vdir"/*; do
        b=$(basename "$f")
        if [ -f "$PREFIX/bin/$b" ] && [ ! -L "$PREFIX/bin/$b" ]; then
            say "replacing $PREFIX/bin/$b, an install from before versions were kept"
        fi
        # A link made beside the target and renamed over it: replacing a
        # running binary in place fails with ETXTBSY, and rename does not.
        ln -sf "../share/$NAME/versions/$TAG/$b" "$PREFIX/bin/.$b.new"
        mv -f "$PREFIX/bin/.$b.new" "$PREFIX/bin/$b"
    done
}

# ---------------------------------------------------------------- depth: the commands
do_install() {
    TAG=$(resolve "$TARGET")
    default_prefix
    vdir="$PREFIX/share/$NAME/versions/$TAG"
    mkdir -p "$PREFIX/share/$NAME/versions" 2>/dev/null && [ -w "$PREFIX/share/$NAME/versions" ] \
        || die "$PREFIX is not writable; pass --prefix \$HOME/.local, or run as root"
    tmp=$(mktemp -d "$PREFIX/share/$NAME/versions/.$TAG.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT INT TERM
    download "$tmp"
    "$tmp/$NAME" $SELFTEST >/dev/null 2>&1 || die "the downloaded $NAME does not run here ($SOURCE)"
    rm -rf "$vdir"
    mv "$tmp" "$vdir"
    trap - EXIT INT TERM
    link_version
    say "installed $("$PREFIX/bin/$NAME" $SELFTEST) to $PREFIX/bin/$NAME"
    say "  source: $SOURCE"
    # What the binary carries, asked of it rather than read off its name.
    "$PREFIX/bin/$NAME" buildinfo 2>/dev/null | sed 's/^/  /' || true
    case ":$PATH:" in *":$PREFIX/bin:"*) ;; *) say "note: $PREFIX/bin is not on your PATH" ;; esac
}

do_get() {
    TAG=$(resolve "$TARGET")
    tmp=$(mktemp -d "./.$NAME-get.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT INT TERM
    download "$tmp"
    for f in "$tmp"/*; do
        mv -f "$f" "./$(basename "$f")"
        say "got ./$(basename "$f") ($TAG, from $SOURCE)"
    done
}

do_list() {
    # The release log first: it is the one source that has dev builds. The
    # mirror's index next, then the changelog on main.
    rows=$(release_log | cut -f1,2 || true)
    [ -n "$rows" ] || rows=$(fetchout "$MIRROR/releases.json" | json_releases || true)
    [ -n "$rows" ] || rows=$(fetchout "$RAW/main/changelog.json" | json_releases || true)
    [ -n "$rows" ] || die "cannot list releases: the release log, the mirror and GitHub all failed"
    if [ "$1" = stable ]; then
        printf '%s\n' "$rows" | awk -F'\t' '$2 == "stable" { print $1 }'
    else
        printf '%s\n' "$rows" | awk -F'\t' '{ printf "%-24s %s\n", $1, $2 }'
    fi
}

do_check() {
    found=0
    for p in $(installed_prefixes); do
        active=$(active_in "$p")
        for v in "$p/share/$NAME/versions"/*; do
            [ -d "$v" ] || continue
            found=1
            t=$(basename "$v")
            if [ "$t" = "$active" ]; then say "$t  $p  (in use)"; else say "$t  $p"; fi
        done
    done
    [ "$found" = 1 ] || say "no release installed by this script"
    on_path=$(command -v "$NAME" 2>/dev/null || true)
    if [ -n "$on_path" ]; then
        say "on PATH: $on_path ($("$on_path" $SELFTEST 2>/dev/null || echo "does not run"))"
        case "$(readlink "$on_path" 2>/dev/null || true)" in
            */share/$NAME/versions/*) ;;
            *) say "  not managed by this script" ;;
        esac
    fi
}

do_use() {
    TAG=$(resolve "$TARGET")
    for p in $(installed_prefixes); do
        if [ -d "$p/share/$NAME/versions/$TAG" ]; then
            PREFIX="$p"; link_version
            say "using $TAG: $PREFIX/bin/$NAME"
            return
        fi
    done
    die "$TAG is not installed (see '$SELF check'; '$SELF install $TAG' installs it)"
}

do_remove() {
    removed=0
    for p in $(installed_prefixes); do
        active=$(active_in "$p")
        if [ -n "$TARGET" ]; then
            TAG=$(resolve "$TARGET")
            [ -d "$p/share/$NAME/versions/$TAG" ] || continue
            rm -rf "$p/share/$NAME/versions/$TAG"
            say "removed $TAG from $p"
            removed=1
            if [ "$TAG" = "$active" ]; then
                for f in "$p/bin"/*; do
                    case "$(readlink "$f" 2>/dev/null || true)" in
                        */share/$NAME/versions/$TAG/*) rm -f "$f" ;;
                    esac
                done
                say "$TAG was in use; '$SELF use VERSION' picks another"
            fi
        else
            for f in "$p/bin"/*; do
                case "$(readlink "$f" 2>/dev/null || true)" in
                    */share/$NAME/versions/*) rm -f "$f" ;;
                esac
            done
            rm -rf "$p/share/$NAME"
            say "removed every $NAME release from $p"
            removed=1
        fi
    done
    [ "$removed" = 1 ] || die "${TARGET:-no release} is not installed (see '$SELF check')"
}

dispatch
