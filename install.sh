#!/bin/sh
# Install DRT — the Diluvium RunTime.
#
#   curl -fsSL https://diluvium.aloecraft.org/release/drt/install.sh | sh
#
# Two sources, and the script says which one it used. The mirror is
# preferred — it is ours, Lab and the deploy scripts read it, and it
# carries `latest/` as a stable path. GitHub Releases is the fallback and
# needs no server-side work, which is why it is also what the README hands
# a stranger today.
#
# Both sources publish SHA256SUMS.txt beside the binaries, so BOTH are
# verified. An earlier version of this script verified only the mirror and
# said so in a comment; that comment was true when the release workflow did
# not upload its sums file, and stopped being true the day it did.
#
# Knobs: DRT_SLIM=1 installs the slim profile; DRT_VERSION=vX.Y.Z pins a
# release; DRT_PREFIX overrides the install directory; DRT_MIRROR points at
# a different mirror (an internal one, or a directory you serve yourself).
set -eu

MIRROR="${DRT_MIRROR:-https://diluvium.aloecraft.org/release/drt}"
GITHUB="https://github.com/Aloecraft-org/diluvium-drt/releases"
VERSION="${DRT_VERSION:-latest}"

case "$(uname -s)" in
  Linux)  OS=linux_static ;;
  Darwin) OS=darwin ;;
  *) echo "install.sh: $(uname -s) has no prebuilt DRT yet; build it with 'cargo build --release --features full -p drt'" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64 ;;
  arm64|aarch64) ARCH=arm64 ;;
  *) echo "install.sh: $(uname -m) has no prebuilt DRT yet" >&2; exit 1 ;;
esac

# Refuse by name rather than by handing over a binary that cannot exec.
# Linux ships x86_64 only today (doc/Release.md: aarch64 is next, and it is
# not promised because `full` carries aws-lc-sys through russh). Before
# this, an aarch64 Linux box downloaded the x86_64 static musl binary and
# failed the --version guard below with "does not run here" — which is true
# and tells you nothing about why.
if [ "$OS" = linux_static ] && [ "$ARCH" != x86_64 ]; then
  echo "install.sh: linux $ARCH has no prebuilt DRT yet — only x86_64." >&2
  echo "  build it: cargo build --release --features full -p drt" >&2
  exit 1
fi

ASSET="drt_${OS}_${ARCH}"
[ "${DRT_SLIM:-}" = 1 ] && ASSET="drt_slim_${OS}_${ARCH}"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fetch() {
  # $1 = url, $2 = out; fails quietly so the caller can decide what to say.
  curl -fsSL "$1" -o "$2" 2>/dev/null
}

sha256_of() {
  (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | cut -d' ' -f1
}

# Resolve a base URL that has the asset, mirror first.
BASE=""
if fetch "$MIRROR/$VERSION/$ASSET" "$TMP/drt"; then
  BASE="$MIRROR/$VERSION"
else
  if [ "$VERSION" = latest ]; then
    BASE="$GITHUB/latest/download"
  else
    BASE="$GITHUB/download/$VERSION"
  fi
  fetch "$BASE/$ASSET" "$TMP/drt" || {
    echo "install.sh: no $ASSET at $MIRROR/$VERSION or $BASE" >&2
    echo "  the published assets are listed at $GITHUB" >&2
    exit 1
  }
fi

# Verify against the sums file that sits beside whichever source answered.
# A missing sums file is a warning rather than a refusal: it means the
# source is older than the sums-publishing workflow, and refusing would
# strand exactly the people trying to install a pinned older release.
if fetch "$BASE/SHA256SUMS.txt" "$TMP/sums"; then
  WANT=$(grep " $ASSET\$" "$TMP/sums" | cut -d' ' -f1)
  HAVE=$(sha256_of "$TMP/drt")
  if [ -z "$WANT" ]; then
    echo "install.sh: $BASE/SHA256SUMS.txt does not list $ASSET; skipping verification" >&2
    VERIFIED="unverified (asset not listed in SHA256SUMS.txt)"
  elif [ "$WANT" != "$HAVE" ]; then
    echo "install.sh: checksum mismatch for $ASSET" >&2
    echo "  expected $WANT" >&2
    echo "  got      $HAVE" >&2
    echo "  from     $BASE" >&2
    exit 1
  else
    VERIFIED="sha256 ok"
  fi
else
  echo "install.sh: $BASE has no SHA256SUMS.txt; skipping verification" >&2
  VERIFIED="unverified (no SHA256SUMS.txt at the source)"
fi

chmod +x "$TMP/drt"
"$TMP/drt" --version >/dev/null 2>&1 || {
  echo "install.sh: the downloaded binary does not run here" >&2
  echo "  $ASSET from $BASE" >&2
  exit 1
}

DEST="${DRT_PREFIX:-}"
if [ -z "$DEST" ]; then
  if [ -w /usr/local/bin ]; then DEST=/usr/local/bin; else DEST="$HOME/.local/bin"; fi
fi
mkdir -p "$DEST"
mv "$TMP/drt" "$DEST/drt"

echo "installed $("$DEST/drt" --version) to $DEST/drt"
echo "  source:  $BASE/$ASSET"
echo "  checked: $VERIFIED"
# What this binary carries, asked of the binary rather than inferred from
# its filename: the diluvium revision inside it, the dv ABI it speaks, and
# the connectors compiled in.
"$DEST/drt" buildinfo 2>/dev/null | sed 's/^/  /' || true
case ":$PATH:" in *":$DEST:"*) ;; *) echo "note: $DEST is not on your PATH" ;; esac
