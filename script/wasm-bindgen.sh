#!/bin/sh
# wasm-bindgen.sh -- print the path of a wasm-bindgen CLI at the version a
# crate pins, fetching the pinned release if nothing at hand matches. The
# glue's format is the crate's: a CLI of another version refuses the
# module, or worse, accepts it and generates glue that does not match.
#
#   script/wasm-bindgen.sh 0.2.127                     # prints the path
#   WASM_BINDGEN=$(script/wasm-bindgen.sh "$pin")
#
# env:  WASM_BINDGEN     use this one; a version other than the pin is an
#                        error rather than a download, since it was named
#       XDG_CACHE_HOME   where a fetched one lives (default ~/.cache)
#
# The one on PATH is used when it is the pinned version, so a person who
# keeps another version there for another project (aloelite pins its own)
# keeps it: the fetched one lives beside the wasi-sdk, under
# ~/.cache/drt/, and nothing is installed over anything.
set -eu

pin=${1:?usage: wasm-bindgen.sh <version>}
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}/drt

version_of() {
    "$1" --version 2>/dev/null | sed -n 's/^wasm-bindgen \([0-9.]*\).*/\1/p'
}

if [ -n "${WASM_BINDGEN:-}" ]; then
    have=$(version_of "$WASM_BINDGEN") || true
    if [ "$have" != "$pin" ]; then
        printf 'wasm-bindgen.sh: WASM_BINDGEN=%s is %s, the crate pins %s\n' "$WASM_BINDGEN" "${have:-missing}" "$pin" >&2
        exit 2
    fi
    printf '%s\n' "$WASM_BINDGEN"
    exit 0
fi

if on_path=$(command -v wasm-bindgen 2>/dev/null) && [ "$(version_of "$on_path")" = "$pin" ]; then
    printf '%s\n' "$on_path"
    exit 0
fi

cached=$CACHE/wasm-bindgen-$pin/wasm-bindgen
if [ -x "$cached" ] && [ "$(version_of "$cached")" = "$pin" ]; then
    printf '%s\n' "$cached"
    exit 0
fi

case $(uname -s)-$(uname -m) in
    Linux-x86_64|Linux-amd64)   triple=x86_64-unknown-linux-musl ;;
    Linux-aarch64|Linux-arm64)  triple=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64)              triple=x86_64-apple-darwin ;;
    Darwin-arm64|Darwin-aarch64) triple=aarch64-apple-darwin ;;
    *)
        printf 'wasm-bindgen.sh: no wasm-bindgen release for %s %s; install one and set WASM_BINDGEN:\n' "$(uname -s)" "$(uname -m)" >&2
        printf '    cargo install wasm-bindgen-cli --version %s --locked\n' "$pin" >&2
        exit 2
        ;;
esac

name=wasm-bindgen-$pin-$triple
url=https://github.com/wasm-bindgen/wasm-bindgen/releases/download/$pin/$name.tar.gz
printf 'wasm-bindgen.sh: fetching wasm-bindgen %s into %s\n' "$pin" "$CACHE" >&2
mkdir -p "$CACHE"
tmp=$(mktemp -d "$CACHE/fetch.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/cli.tar.gz" "$url"
tar -C "$tmp" -xzf "$tmp/cli.tar.gz"
# A half-extracted tree never sits at the final path: the move is last.
rm -rf "$CACHE/wasm-bindgen-$pin"
mv "$tmp/$name" "$CACHE/wasm-bindgen-$pin"
printf '%s\n' "$cached"
