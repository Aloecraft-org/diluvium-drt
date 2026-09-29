#!/bin/sh
# The SSH page's gate (doc/Plan-0.8.0.md §2.2): build dist/ssh.html, build
# `drt` with `full`, then crates/drt-ssh-web/page/e2e.mjs -- stock sshd,
# the relay, a parked device, stock `ssh` over ProxyCommand and the page in
# Chromium, eight checks. CI and the release both run this, so the page
# that ships is the one that passed.
#
#   script/drt-ssh-page-gate.sh [--package DIR]
#
# Needs: the wasm32-unknown-unknown target, the wasm-bindgen CLI at the
# pinned version (WASM_BINDGEN, as script/drt-ssh-page.sh reads it), node
# 22, Chromium for the page's pinned Playwright, and an sshd it can run as
# root: directly when this is root, through `sudo -n` otherwise.
#
# --package DIR copies dist/ssh.html into DIR once every check passed.
set -eu
cd "$(dirname "$0")/.."

OUT=
while [ $# -gt 0 ]; do
    case "$1" in
        --package) OUT=$2; shift ;;
        *) echo "usage: $0 [--package DIR]" >&2; exit 2 ;;
    esac
    shift
done

script/drt-ssh-page.sh
cargo build -p drt --features full
(cd crates/drt-ssh-web/page && node e2e.mjs)

if [ -n "$OUT" ]; then
    mkdir -p "$OUT"
    cp crates/drt-ssh-web/page/dist/ssh.html "$OUT/"
fi
