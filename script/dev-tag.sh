#!/bin/sh
# The next free dev tag (doc/ALIGNMENT.md §7): v<version>-dev.<n>, where
# <version> is .technoproj's major.minor.patch and <n> is one more than the
# highest -dev. number any tag in this repository has ever carried. The
# counter is global and never reused, so a number names one build forever;
# it is allocated from the tags that exist rather than stored in the tree,
# so a nightly commits nothing and two branches cannot collide.
#
#   script/dev-tag.sh               prints the tag, e.g. v0.6.0-dev.3
#   script/dev-tag.sh --if-changed  prints nothing and exits 3 when HEAD is
#                                   the commit the newest dev tag points at,
#                                   so a nightly does not cut one build twice
#
# Dispatch the Release workflow with the printed tag and publish=true, or
# push the tag; either way the suffix selects the fast path.
set -eu
cd "$(dirname "$0")/.."

# The tags that exist, not the ones this clone happened to have.
git fetch --tags -q origin 2>/dev/null || true

version=$(python3 -c '
import json
v = json.load(open(".technoproj"))["TECHNO_VERSION"]
print("%d.%d.%d" % (v["major"], v["minor"], v["patch"]))
')
last=$(git tag --list 'v*-dev.*' | sed -n 's/.*-dev\.\([0-9][0-9]*\)$/\1/p' | sort -n | tail -1)

if [ "${1:-}" = --if-changed ] && [ -n "$last" ]; then
    newest=$(git tag --list "v*-dev.$last" | head -1)
    if [ "$(git rev-parse "$newest^{commit}")" = "$(git rev-parse HEAD)" ]; then
        exit 3
    fi
fi

echo "v$version-dev.$(( ${last:-0} + 1 ))"
