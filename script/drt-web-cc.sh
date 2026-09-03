#!/usr/bin/env bash
#
# drt-web-cc.sh -- the C compiler for the browser module: the wasi-sdk's
# clang, minus one flag diluvium-sys adds for wasm32-unknown-unknown.
#
# diluvium-sys compiles the C core for the browser with -DLUA_USE_C89,
# from the days when that target had no libc: C89 numbers make lua_Integer
# a `long`, which is 32 bits on wasm32, and a millisecond timestamp does
# not fit one -- `host.time()` fails in the page and passes everywhere
# else. The browser module links wasi-libc now (doc/Wasm.md D4), the same
# libc the wasip2 build has, so the core can be the same C99 build there
# too. Until diluvium-sys drops the flag for this target (the ask is in
# doc/Wasm.md §7), drt-web.sh names this script as CC for the target and
# it removes the flag. Delete it the day the ask lands.
#
# env: WASI_SDK_PATH  the wasi-sdk whose clang this runs

set -eu
: "${WASI_SDK_PATH:?drt-web-cc.sh needs WASI_SDK_PATH}"
args=()
for a in "$@"; do
    [ "$a" = "-DLUA_USE_C89" ] || args+=("$a")
done
exec "$WASI_SDK_PATH/bin/clang" "${args[@]}"
