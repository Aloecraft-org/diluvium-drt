#!/bin/sh
# wasi-sdk.sh -- print the path of a wasi-sdk, fetching the pinned one if
# none is named. The browser module (script/drt-web.sh) and the
# wasm32-wasip2 build compile the C core with it; the native build does
# not need one, so nothing on the host has one to borrow.
#
#   script/wasi-sdk.sh                                  # prints the path
#   WASI_SDK_PATH=$(script/wasi-sdk.sh) cargo build --target wasm32-wasip2 ...
#
# env:  WASI_SDK_PATH    use this one, fetch nothing
#       XDG_CACHE_HOME   where the fetched one lives (default ~/.cache)
#
# The version is the one CI uses (.github/workflows/ci.yml); the
# release's own tarballs are fetched, one per OS and architecture.
set -eu

VERSION=27.0
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/drt

if [ -n "${WASI_SDK_PATH:-}" ]; then
    printf '%s\n' "$WASI_SDK_PATH"
    exit 0
fi

case $(uname -s) in
    Linux)  os=linux ;;
    Darwin) os=macos ;;
    *) printf 'wasi-sdk.sh: no wasi-sdk release for %s; set WASI_SDK_PATH\n' "$(uname -s)" >&2; exit 2 ;;
esac
case $(uname -m) in
    x86_64|amd64)  arch=x86_64 ;;
    aarch64|arm64) arch=arm64 ;;
    *) printf 'wasi-sdk.sh: no wasi-sdk release for %s; set WASI_SDK_PATH\n' "$(uname -m)" >&2; exit 2 ;;
esac

name=wasi-sdk-$VERSION-$arch-$os
sdk=$CACHE/$name
if [ -x "$sdk/bin/clang" ]; then
    printf '%s\n' "$sdk"
    exit 0
fi

major=${VERSION%%.*}
url=https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$major/$name.tar.gz
printf 'wasi-sdk.sh: fetching %s into %s\n' "$name" "$CACHE" >&2
mkdir -p "$CACHE"
tmp=$(mktemp -d "$CACHE/fetch.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/sdk.tar.gz" "$url"
tar -C "$tmp" -xzf "$tmp/sdk.tar.gz"
# A half-extracted tree never sits at the final path: the move is last.
mv "$tmp/$name" "$sdk"
printf '%s\n' "$sdk"
