#!/usr/bin/env python3
"""The consistency checks that are this repository's alone.

`doc/ALIGNMENT.md` §3: the shared changelog engine reproduces what every
repository has in common from a declaration in `.technoproj`, and calls
`script/checks.py:consistency(doc, ctx)` for the invariants that do not
generalise. This is DRT's one: the diluvium revision the changelog claims
is the one `Cargo.lock` pins, because BUILDINFO's neighbour would otherwise
carry a wrong number.

`ctx` carries `read` (a path under the repository root -> its text), `root`
(the repository root) and `base` (the newest entry's `X.Y.Z`). Until the
shared engine lands, `script/changelog.py` supplies the same `ctx` and
calls this from its own `consistency`.
"""

import re


def consistency(doc, ctx):
    """-> list of problems, empty when the tree agrees with the entry."""
    bad = []
    r = doc["releases"][0]
    where = "newest entry (%s)" % r["version"]
    if not r.get("diluvium"):
        return bad
    try:
        lock = ctx.read("Cargo.lock")
    except OSError as e:
        return ["Cargo.lock: cannot read (%s)" % e]
    m = re.search(
        r'name = "diluvium"\nversion = "[^"]*"\n'
        r'source = "git\+[^#]*#([0-9a-f]+)"', lock)
    if not m:
        bad.append("Cargo.lock: no git revision pinned for diluvium")
    elif not m.group(1).startswith(str(r["diluvium"])[:12]):
        bad.append("Cargo.lock pins diluvium %s but %s says %s"
                   % (m.group(1)[:12], where, str(r["diluvium"])[:12]))
    return bad
