#!/bin/sh
# Install DRT — the Diluvium RunTime.
#
#   curl -fsSL https://diluvium.aloecraft.org/release/drt/install.sh | sh
#
# Mirror-first (the same release mirror Lab and the deploy scripts read),
# GitHub Releases as the fallback. Verifies the SHA-256 when the mirror's
# sums file is reachable. DRT_SLIM=1 installs the slim profile;
# DRT_VERSION=vX.Y.Z pins a release; DRT_PREFIX overrides the directory.
set -eu

MIRROR="${DRT_MIRROR:-https://diluvium.aloecraft.org/release/drt}"
GITHUB="https://github.com/Aloecraft-org/diluvium-drt/releases"
VERSION="${DRT_VERSION:-latest}"

case "$(uname -s)" in
  Linux)  OS=linux_static ;;
  Darwin) OS=darwin ;;
  *) echo "install.sh: $(uname -s) has no prebuilt DRT yet; cargo build --features full -p drt" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64 ;;
  arm64|aarch64) ARCH=arm64 ;;
  *) echo "install.sh: $(uname -m) has no prebuilt DRT yet" >&2; exit 1 ;;
esac
# The static Linux build ships x86_64 today; darwin ships both.
[ "$OS" = linux_static ] && ARCH=x86_64
[ "$OS" = darwin ] && [ "$ARCH" = x86_64 ] && ARCH=x86_64

ASSET="drt_${OS}_${ARCH}"
[ "${DRT_SLIM:-}" = 1 ] && ASSET="drt_slim_${OS}_${ARCH}"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fetch() {
  # $1 = url, $2 = out; fails quietly so the fallback can speak.
  curl -fsSL "$1" -o "$2" 2>/dev/null
}

GOT=""
if fetch "$MIRROR/$VERSION/$ASSET" "$TMP/drt"; then
  GOT="$MIRROR/$VERSION"
  # Verify against the mirror's sums when it has them. A missing sums file
  # is a warning, not a refusal — the mirror is ours, and the fallback path
  # below has no sums at all.
  if fetch "$GOT/SHA256SUMS.txt" "$TMP/sums"; then
    WANT=$(grep " $ASSET\$" "$TMP/sums" | cut -d' ' -f1)
    HAVE=$( (sha256sum "$TMP/drt" 2>/dev/null || shasum -a 256 "$TMP/drt") | cut -d' ' -f1)
    if [ -n "$WANT" ] && [ "$WANT" != "$HAVE" ]; then
      echo "install.sh: checksum mismatch for $ASSET from the mirror" >&2
      exit 1
    fi
  else
    echo "install.sh: mirror has no SHA256SUMS.txt for $VERSION; skipping verification" >&2
  fi
else
  URL="$GITHUB/latest/download/$ASSET"
  [ "$VERSION" != latest ] && URL="$GITHUB/download/$VERSION/$ASSET"
  fetch "$URL" "$TMP/drt" || {
    echo "install.sh: no $ASSET at the mirror or $GITHUB" >&2
    exit 1
  }
  GOT="$URL"
fi

chmod +x "$TMP/drt"
"$TMP/drt" --version >/dev/null || { echo "install.sh: the downloaded binary does not run here" >&2; exit 1; }

DEST="${DRT_PREFIX:-}"
if [ -z "$DEST" ]; then
  if [ -w /usr/local/bin ]; then DEST=/usr/local/bin; else DEST="$HOME/.local/bin"; fi
fi
mkdir -p "$DEST"
mv "$TMP/drt" "$DEST/drt"
echo "installed $($DEST/drt --version) to $DEST/drt (from $GOT)"
case ":$PATH:" in *":$DEST:"*) ;; *) echo "note: $DEST is not on your PATH" ;; esac
