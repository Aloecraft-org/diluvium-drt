#!/bin/sh
# drt.js -- one ES module a page imports from any origin, with no sibling
# files: the browser access client, the SSH client, the browser build of
# drt and both wasm modules inlined (crates/drt-web/cdn). Writes
# crates/drt-web/cdn/dist/drt.js and drt.min.js.
#
#   script/drt-js.sh [VERSION]
#
# Expects what script/drt-web.sh and script/drt-ssh-page.sh leave behind:
# each module and its inline glue under crates/drt-web/browser-test/pkg and
# crates/drt-ssh-web/page/pkg. Needs node 22 and npm; the directory's own
# package.json pins esbuild.
set -eu
cd "$(dirname "$0")/../crates/drt-web/cdn"
[ -d node_modules ] || npm ci --silent
node build.mjs "${1:-dev}"
