#!/bin/sh
# The browser access client library's gate (crates/drt-rtc/client/): its
# unit tests against the shared vectors, then Chromium driving it against a
# real host -- the drt-rtc example host, and with --drt a whole `drt start`
# too. CI and the release both run this, so what ships is what was proven.
#
#   script/browser-access-client.sh [--drt] [--package DIR]
#
# Needs node 22 and Playwright with Chromium. PLAYWRIGHT names the
# playwright package directory; the default is the one
# crates/drt-web/browser-test pins, after `npm ci` there.
#
# --package DIR copies the two shipped files into DIR once everything
# passed, so a release packages exactly the bytes that were tested.
set -eu
cd "$(dirname "$0")/.."

DRT=
OUT=
while [ $# -gt 0 ]; do
    case "$1" in
        --drt) DRT=1 ;;
        --package) OUT=$2; shift ;;
        *) echo "usage: $0 [--drt] [--package DIR]" >&2; exit 2 ;;
    esac
    shift
done
: "${PLAYWRIGHT:=$PWD/crates/drt-web/browser-test/node_modules/playwright}"
export PLAYWRIGHT

node --test crates/drt-rtc/client/test.mjs

cargo build -p drt-rtc --example browser_check
(cd crates/drt-rtc/browser-check && SESSIONS=3 node check.mjs)
if [ -n "$DRT" ]; then
    cargo build -p drt --features full
    (cd crates/drt-rtc/browser-check && SESSIONS=2 node check.mjs --drt)
fi

if [ -n "$OUT" ]; then
    mkdir -p "$OUT"
    cp crates/drt-rtc/client/drt_browser_access.js crates/drt-rtc/client/drt_browser_access.d.ts "$OUT/"
fi
