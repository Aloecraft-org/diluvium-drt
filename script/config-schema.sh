#!/usr/bin/env bash
# The config file's JSON Schema, generated from drt-config's serde types
# into doc/drt-config.schema.json. `--check` fails when the checked-in copy
# is stale, which is how CI holds the two together. The browser package
# (script/drt-web-package.sh) ships the same file as config.schema.json.
set -eu
HERE=$(cd -- "$(dirname -- "$0")/.." && pwd)
OUT=$HERE/doc/drt-config.schema.json
fresh=$(cd "$HERE" && cargo run -q -p drt-config --features schemars --example schema)
if [ "${1:-}" = "--check" ]; then
    if [ "$fresh" != "$(cat "$OUT")" ]; then
        echo "config-schema.sh: $OUT is stale; run script/config-schema.sh" >&2
        exit 1
    fi
    echo "config-schema.sh: $OUT is current"
else
    printf '%s\n' "$fresh" > "$OUT"
    echo "config-schema.sh: wrote $OUT"
fi
