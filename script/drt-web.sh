#!/usr/bin/env bash
#
# drt-web.sh -- build the browser module and generate its glue
# (doc/Wasm.md §4.4, §8): drt_web_bg.wasm and drt_web.js under
# crates/drt-web/browser-test/pkg, where index.html and run.mjs load them.
#
# usage: script/drt-web.sh [out-dir]
# env:   WASI_SDK_PATH   a wasi-sdk (>= 24; 27 verified): the C core and
#                        wasi-libc are compiled and linked from it
#        WASM_BINDGEN    the wasm-bindgen CLI (default: on PATH). Its
#                        version must equal the crate's pin in
#                        crates/drt-web/Cargo.toml; the check is below.
#        DRT_WEB_PROFILE cargo profile (default: release; release-small
#                        for the artifact that ships)
#        CARGO_TARGET_DIR  as cargo reads it

set -eu

HERE=$(cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:-$HERE/crates/drt-web/browser-test/pkg}
PROFILE=${DRT_WEB_PROFILE:-release}
WASM_BINDGEN=${WASM_BINDGEN:-wasm-bindgen}
TARGET_DIR=${CARGO_TARGET_DIR:-$HERE/target}

: "${WASI_SDK_PATH:?set WASI_SDK_PATH to a wasi-sdk; the browser module links the C core and wasi-libc from it (doc/Wasm.md §8)}"

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

# The C core, compiled without the 32-bit-integer flag diluvium-sys still
# passes for this target (see drt-web-cc.sh). Both variables, because
# diluvium-sys takes an explicit CC as the whole toolchain and then looks
# for `llvm-ar` on PATH. Cargo does not rerun diluvium-sys's build script
# when only these change, so after editing drt-web-cc.sh:
#     cargo clean -p diluvium-sys --target wasm32-unknown-unknown
export CC_wasm32_unknown_unknown=${CC_wasm32_unknown_unknown:-$HERE/script/drt-web-cc.sh}
export AR_wasm32_unknown_unknown=${AR_wasm32_unknown_unknown:-$WASI_SDK_PATH/bin/llvm-ar}

cargo build -p drt-web --target wasm32-unknown-unknown --profile "$PROFILE"
module=$TARGET_DIR/wasm32-unknown-unknown/$dir/drt_web.wasm
"$WASM_BINDGEN" --target web --out-dir "$OUT" --out-name drt_web "$module"
printf 'drt-web.sh: %s (%s bytes)\n' "$OUT/drt_web_bg.wasm" "$(wc -c < "$OUT/drt_web_bg.wasm")"
