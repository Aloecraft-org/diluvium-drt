#!/bin/sh
# drt.js's gate: build it from the modules already built, build `drt` with
# `full`, then crates/drt-web/cdn/e2e.mjs -- Chromium on one origin
# importing drt.js from another and calling a parked peer, SSH to a stock
# sshd over it, a stream, the kv store and the browser build. CI and the
# release both run this, so the file that ships is the one that passed.
# Then get-drt.test.mjs, get-drt.js's check, over the drt.min.js built here.
#
#   script/drt-js-gate.sh [--version V] [--package DIR]
#
# Needs: script/drt-web.sh and script/drt-ssh-page.sh run first, node 22,
# Chromium for the directory's pinned Playwright, and an sshd it can run
# as root: directly when this is root, through `sudo -n` otherwise. DRT
# names a native drt with `full` to use instead of building one.
#
# --package DIR copies dist/drt.js and dist/drt.min.js into DIR once every
# check passed.
set -eu
cd "$(dirname "$0")/.."

OUT=
VERSION=dev
while [ $# -gt 0 ]; do
    case "$1" in
        --package) OUT=$2; shift ;;
        --version) VERSION=$2; shift ;;
        *) echo "usage: $0 [--version V] [--package DIR]" >&2; exit 2 ;;
    esac
    shift
done

script/drt-js.sh "$VERSION"
# DRT names a native drt already built with `full`; else one is built.
if [ -z "${DRT:-}" ]; then
    cargo build -p drt --features full
    DRT=$PWD/target/debug/drt
fi
(cd crates/drt-web/cdn && DRT=$DRT node e2e.mjs)
# get-drt.js, the release asset that loads a published drt.js into a page:
# against a fake mirror and Pages site, then with the drt.min.js just built.
(cd crates/drt-web/cdn && node get-drt.test.mjs)

if [ -n "$OUT" ]; then
    mkdir -p "$OUT"
    cp crates/drt-web/cdn/dist/drt.js crates/drt-web/cdn/dist/drt.min.js "$OUT/"
fi
