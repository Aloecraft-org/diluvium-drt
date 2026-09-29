#!/usr/bin/env bash
#
# drt-ssh-page.sh -- build the single-file SSH page (doc/Plan-0.8.0.md §2):
# the drt-ssh-web module, its wasm-bindgen glue, xterm.js and the page, all
# inlined into crates/drt-ssh-web/page/dist/ssh.html. One file, no server
# needed beyond whatever serves it, and nothing fetched at run time.
#
# usage: script/drt-ssh-page.sh
# env:   WASM_BINDGEN    the wasm-bindgen CLI (default: on PATH), at the
#                        version crates/drt-ssh-web pins; checked below
#        DRT_SSH_PROFILE cargo profile (default: release-small)
#        CARGO_TARGET_DIR  as cargo reads it
#
# Needs node and npm: the page's own package.json pins xterm.js and the
# binaryen that runs wasm-opt.

set -eu

HERE=$(cd -- "$(dirname -- "$0")/.." && pwd)
PAGE=$HERE/crates/drt-ssh-web/page
PROFILE=${DRT_SSH_PROFILE:-release-small}
WASM_BINDGEN=${WASM_BINDGEN:-wasm-bindgen}
TARGET_DIR=${CARGO_TARGET_DIR:-$HERE/target}

# The glue's format is the crate's: a CLI of another version refuses the
# module, or accepts it and generates glue that does not match.
pin=$(sed -n 's/^wasm-bindgen = "=\([0-9.]*\)"/\1/p' "$HERE/crates/drt-ssh-web/Cargo.toml")
have=$("$WASM_BINDGEN" --version 2>/dev/null | sed -n 's/^wasm-bindgen \([0-9.]*\).*/\1/p') || true
if [ "$have" != "$pin" ]; then
    printf 'drt-ssh-page.sh: need wasm-bindgen %s (WASM_BINDGEN=%s is %s)\n' "$pin" "$WASM_BINDGEN" "${have:-missing}" >&2
    printf '    cargo install wasm-bindgen-cli --version %s --locked\n' "$pin" >&2
    exit 2
fi

case $PROFILE in
    dev) dir=debug ;;
    *)   dir=$PROFILE ;;
esac

# Unstripped at the cargo level, for drt-web.sh's reason: wasm-bindgen
# reads symbols the strip would take. wasm-opt below does the shrinking.
strip_key=CARGO_PROFILE_$(printf '%s' "$PROFILE" | tr 'a-z-' 'A-Z_')_STRIP
env "$strip_key=none" cargo build -p drt-ssh-web --target wasm32-unknown-unknown --profile "$PROFILE"

# `no-modules`, so the glue is a classic script that inlines into the page
# beside xterm.js and defines the one global, `wasm_bindgen`.
"$WASM_BINDGEN" --target no-modules --no-typescript --out-dir "$PAGE/pkg" --out-name drt_ssh_web \
    "$TARGET_DIR/wasm32-unknown-unknown/$dir/drt_ssh_web.wasm"

cd "$PAGE"
[ -d node_modules ] || npm ci --silent
npx --no-install wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int \
    --enable-sign-ext --enable-mutable-globals --enable-reference-types \
    pkg/drt_ssh_web_bg.wasm -o pkg/drt_ssh_web_bg.wasm
node build.mjs
