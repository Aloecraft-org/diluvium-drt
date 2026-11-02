#!/bin/sh
# Is COMMIT a production release? The dev-build triggers ask this so a
# release commit gets its release and not a dev build beside it.
#
#   script/release-commit.sh [COMMIT]   prints the release tag and exits 0
#                                       when COMMIT is a release commit;
#                                       exits 3 when it is not
#
# COMMIT defaults to HEAD. A release commit is one whose newest changelog
# entry is released (or tagged) and either:
#   - that entry's tag already points at COMMIT, or
#   - the tag does not exist yet and COMMIT is where the entry became
#     released, i.e. its first parent's newest entry was a different tag
#     or still unreleased. This is the merge of a release-prep PR, which
#     lands on main before the owner pushes the tag.
# A later merge while the tag is still pending is not a release commit,
# so it still gets its dev build.
#
# Reads the committed changelog.json, not CHANGELOG.yaml, so it needs jq
# and git and no PyYAML. Identical in diluvium and diluvium-drt, like
# script/dev-tag.sh.
set -eu
cd "$(dirname "$0")/.."

commit=$(git rev-parse "${1:-HEAD}^{commit}")

# The tag of the newest entry at a commit if it is released, else nothing.
released_tag() {
    git show "$1:changelog.json" 2>/dev/null \
        | jq -r '.releases[0] | select(.status != "unreleased") | .tag' \
        || true
}

tag=$(released_tag "$commit")
[ -n "$tag" ] || exit 3

if at=$(git rev-parse -q --verify "refs/tags/$tag^{commit}"); then
    [ "$at" = "$commit" ] || exit 3
else
    [ "$(released_tag "$commit^1")" != "$tag" ] || exit 3
fi
echo "$tag"
