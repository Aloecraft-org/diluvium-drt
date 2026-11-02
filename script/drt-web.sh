#!/usr/bin/env bash
#
# drt-web.sh -- build the browser module and generate its glue
# (doc/Wasm.md §4.4, §8): drt_web_bg.wasm and drt_web.js under
# crates/drt-web/browser-test/pkg, where index.html and run.mjs load them,
# and inline/drt_web.js, the glue drt.js bundles (script/drt-js.sh).
#
# usage: script/drt-web.sh [out-dir]
# env:   WASI_SDK_PATH   a wasi-sdk (>= 24; 27 verified): the C core and
#                        wasi-libc are compiled and linked from it. Unset,
#                        script/wasi-sdk.sh fetches the pinned one.
#        WASM_BINDGEN    the wasm-bindgen CLI (default: on PATH). Its
#                        version must equal the crate's pin in
#                        crates/drt-web/Cargo.toml; the check is below.
#        DRT_WEB_PROFILE cargo profile (default: release; release-small
#                        for the artifact that ships). Its `strip` is
#                        overridden to none -- see the build below.
#        DRT_WEB_OPT     0 skips wasm-opt (default: run it)
#        CARGO_TARGET_DIR  as cargo reads it

set -eu

HERE=$(cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:-$HERE/crates/drt-web/browser-test/pkg}
PROFILE=${DRT_WEB_PROFILE:-release}
WASM_BINDGEN=${WASM_BINDGEN:-wasm-bindgen}
TARGET_DIR=${CARGO_TARGET_DIR:-$HERE/target}

# The C core and wasi-libc are compiled and linked from a wasi-sdk;
# diluvium-sys and crates/drt-web/build.rs both read the variable.
WASI_SDK_PATH=$("$HERE/script/wasi-sdk.sh")
export WASI_SDK_PATH

# The glue's format is the crate's: a CLI of another version refuses the
# module, or worse, accepts it and generates glue that does not match.
pin=$(sed -n 's/^wasm-bindgen = "=\([0-9.]*\)"/\1/p' "$HERE/crates/drt-web/Cargo.toml")
have=$("$WASM_BINDGEN" --version 2>/dev/null | sed -n 's/^wasm-bindgen \([0-9.]*\).*/\1/p') || true
if [ -z "$have" ]; then
    printf 'drt-web.sh: no wasm-bindgen CLI (WASM_BINDGEN=%s). Install the pinned one:\n' "$WASM_BINDGEN" >&2
    printf '    cargo install wasm-bindgen-cli --version %s --locked\n' "$pin" >&2
    exit 2
fi
if [ "$have" != "$pin" ]; then
    printf 'drt-web.sh: wasm-bindgen CLI is %s, crates/drt-web pins %s; they must match.\n' "$have" "$pin" >&2
    exit 2
fi

case $PROFILE in
    dev) dir=debug ;;
    *)   dir=$PROFILE ;;
esac

# Never stripped at the cargo level, whatever the profile says. wasm-bindgen
# finds the module's externref table through its symbol, and a stripped
# module fails the run with "externref table required for catch wrappers" --
# a module that carries exception-handling instructions, which this one does
# for the C core's `longjmp` (doc/Wasm.md §2.3), needs that table found. The
# artifact does not pay for it: `release-small` unstripped is smaller than
# `release`, because opt-level, LTO and codegen-units do the real work and
# the symbols largely do not survive `wasm-bindgen` anyway.
strip_key=CARGO_PROFILE_$(printf '%s' "$PROFILE" | tr 'a-z-' 'A-Z_')_STRIP
env "$strip_key=none"     cargo build -p drt-web --target wasm32-unknown-unknown --profile "$PROFILE"
module=$TARGET_DIR/wasm32-unknown-unknown/$dir/drt_web.wasm
"$WASM_BINDGEN" --target web --out-dir "$OUT" --out-name drt_web "$module"
# The same glue with no default module path, for drt.js
# (crates/drt-web/cdn), which hands it the bytes and must not name a
# sibling .wasm. The module beside it is the one above, byte for byte.
"$WASM_BINDGEN" --target web --omit-default-module-path --no-typescript \
    --out-dir "$OUT/inline" --out-name drt_web "$module"
cmp -s "$OUT/drt_web_bg.wasm" "$OUT/inline/drt_web_bg.wasm" || {
    echo 'drt-web.sh: the inline glue came with another module; drt.js would not run what was tested' >&2
    exit 1
}
rm "$OUT/inline/drt_web_bg.wasm"

# Shrunk with wasm-opt, as the SSH page's module is: about 5.9 MB to 2.8 MB
# (1.38 MB to 1.11 MB gzipped, measured 2026-11-01), so drt.js and the
# tarball carry the smaller one. After the comparison above, which is
# between wasm-bindgen's two outputs. The features are the ones the module
# uses; exception handling is the C core's `longjmp` (doc/doc_old/Wasm.md
# §2.3). binaryen is pinned in crates/drt-web/browser-test/package.json.
# DRT_WEB_OPT=0 skips it, for a quicker local build.
if [ "${DRT_WEB_OPT:-1}" != 0 ]; then
    (cd "$HERE/crates/drt-web/browser-test" && { [ -d node_modules ] || npm ci --silent; } &&
     npx --no-install wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
        --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-exception-handling \
        "$OUT/drt_web_bg.wasm" -o "$OUT/drt_web_bg.wasm")
fi
printf 'drt-web.sh: %s (%s bytes)\n' "$OUT/drt_web_bg.wasm" "$(wc -c < "$OUT/drt_web_bg.wasm")"
