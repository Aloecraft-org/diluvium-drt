#!/bin/sh
# drt, as the wasm32-wasip2 component under wasmtime (doc/Wasm.md D2, §8).
#
#   script/drt-wasip2.sh run app.dlua
#   cd examples && DRT=../script/drt-wasip2.sh ./run-all.sh
#
# DRT_WASM names the component; the default is the release-small build of
# the `wasi` profile. DRT_WASMTIME_FLAGS adds wasmtime options.
#
# `-W exceptions=y` is not optional. The C core's setjmp/longjmp is lowered
# onto the exception-handling proposal, and without the flag the module is
# refused at load. `--dir .` maps the working directory and nothing else,
# which is what the examples gate and every config path in this repository
# assume: a config naming `./workspace` means the one beside it.
# `-S inherit-network=y -S tcp=y` is what lets a deployment's listener
# bind (doc/Wasm.md M6); nothing else in the wasi profile opens a socket --
# no connector in it reaches out -- so the grant is exactly the listener
# the config asked for, on a port the config names.
set -eu

self=$0
if command -v readlink >/dev/null 2>&1; then
    self=$(readlink -f -- "$0" 2>/dev/null || printf '%s' "$0")
fi
here=$(cd -- "$(dirname -- "$self")" && pwd)
: "${DRT_WASM:=$here/../target/wasm32-wasip2/release-small/drt.wasm}"

if [ ! -f "$DRT_WASM" ]; then
    printf 'drt-wasip2: no component at %s\n' "$DRT_WASM" >&2
    printf '    build one (a wasi-sdk >= 24 named by WASI_SDK_PATH is required):\n' >&2
    printf '        cargo build --profile release-small -p drt --no-default-features --features wasi --target wasm32-wasip2\n' >&2
    printf '    or point DRT_WASM at one.\n' >&2
    exit 2
fi
if ! command -v wasmtime >/dev/null 2>&1; then
    printf 'drt-wasip2: wasmtime is not on PATH (https://wasmtime.dev, 43 or newer)\n' >&2
    exit 2
fi

# shellcheck disable=SC2086  # the flags are several words on purpose
exec wasmtime run -W exceptions=y --dir . -S inherit-network=y -S tcp=y ${DRT_WASMTIME_FLAGS:-} "$DRT_WASM" "$@"
