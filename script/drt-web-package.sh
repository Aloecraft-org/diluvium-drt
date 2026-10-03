#!/usr/bin/env bash
# The browser package: everything a page needs from drt, as one directory
# that is also an npm package, from files already built and tested.
#
# usage: script/drt-web-package.sh OUT_DIR VERSION
#   OUT_DIR  written fresh: the package's files
#   VERSION  the release's, e.g. 0.8.0 or 0.8.0-dev.17
#
# Expects script/drt-web.sh's output under crates/drt-web/browser-test/pkg
# and the SSH page's module under crates/drt-ssh-web/page/pkg, and runs
# cargo for the config schema. The release's Package step calls this and
# tars OUT_DIR as drt_web.tar.gz; `npm pack` in OUT_DIR is the tarball
# npm takes.
set -eu
HERE=$(cd -- "$(dirname -- "$0")/.." && pwd)
OUT=${1:?OUT_DIR}
VERSION=${2:?VERSION}
pkg=$HERE/crates/drt-web/browser-test
rm -rf "$OUT"
mkdir -p "$OUT"
cp "$pkg/pkg/drt_web.js" "$pkg/pkg/drt_web_bg.wasm" "$pkg/pkg/drt_web.d.ts" \
   "$pkg/pkg/drt_web_bg.wasm.d.ts" "$pkg/drt-term.js" "$pkg/shell.js" \
   "$pkg/ssh-terminal.js" "$pkg/relay-leg.js" "$pkg/ssh-command.js" \
   "$HERE/crates/drt-ssh-web/page/pkg/drt_ssh_web.js" \
   "$HERE/crates/drt-ssh-web/page/pkg/drt_ssh_web_bg.wasm" \
   "$HERE/crates/drt-rtc/client/drt_browser_access.js" \
   "$HERE/crates/drt-rtc/client/drt_browser_access.d.ts" \
   "$HERE/crates/drt-web/npm/README.md" \
   "$OUT/"
(cd "$HERE" && cargo run -q -p drt-config --features schemars --example schema) > "$OUT/config.schema.json"
# npm wants a semver; a tag's `v` is not part of one.
sed "s/\"0.0.0-set-by-release\"/\"${VERSION#v}\"/" "$HERE/crates/drt-web/npm/package.json" > "$OUT/package.json"
echo "drt-web-package.sh: $OUT, drt-browser ${VERSION#v}"
