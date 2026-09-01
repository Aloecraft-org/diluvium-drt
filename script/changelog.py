#!/usr/bin/env python3
"""DRT changelog tool.

CHANGELOG.yaml is the source of truth for release notes. This renders it
and checks it, so that the release page, the release mirror and the copy
of CHANGELOG.md in the tree are all derived from one file rather than
maintained in parallel.

Ported from diluvium's `script/changelog.py` deliberately, shape and CLI
intact, so that the two repositories' release machinery reads the same and
a fix to one is a fix a person can carry to the other. What changed is
only what the compatibility facts are: diluvium records `lua_base` and
`bytecode_format`, DRT records `dv_abi`, the embedded `diluvium` revision
and the per-profile `connectors` set -- which is exactly what BUILDINFO.txt
carries, because doc/Release.md's rule is that the compatibility fact
travels with the bytes.

Usage:
  script/changelog.py validate              schema and consistency checks
  script/changelog.py render md             whole changelog, as Markdown
  script/changelog.py render md --tag TAG   one release's section only
                                            (what a release body wants)
  script/changelog.py render json           machine-readable form
  script/changelog.py mirror-tags           tags the mirror should carry,
                                            newest first
  script/changelog.py latest                the tag `latest/` resolves to
  script/changelog.py generate              write CHANGELOG.md and
                                            changelog.json from the YAML
  script/changelog.py check                 fail unless the generated
                                            files match the YAML; for CI
  script/changelog.py consistency           fail unless the tree agrees
                                            with the newest entry; for CI
  script/changelog.py release-check --tag TAG [--publish]
                                            fail unless TAG is releasable;
                                            prints prerelease= and version=
                                            for GITHUB_OUTPUT

Why the generated files are committed: the release mirror runs on a host
with a stdlib-only Python and no build step, so it reads changelog.json
directly. `check` is what stops that copy going stale.

Requires PyYAML (pip install pyyaml).
"""
import argparse
import json
import os
import re
import sys

try:
    import yaml
except ImportError:
    sys.exit("changelog.py: PyYAML is required (pip install pyyaml)")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SOURCE = os.path.join(ROOT, "CHANGELOG.yaml")
MD = os.path.join(ROOT, "CHANGELOG.md")
JSON = os.path.join(ROOT, "changelog.json")

# keepachangelog's six, in the order it prints them, plus our two.
SECTIONS = [
    ("added", "Added"),
    ("changed", "Changed"),
    ("deprecated", "Deprecated"),
    ("removed", "Removed"),
    ("fixed", "Fixed"),
    ("security", "Security"),
    ("known_issues", "Known issues"),
]
STATUSES = {"released", "unreleased", "tagged"}
SCALARS = {"version", "tag", "date", "status", "stable", "latest", "mirror",
           "dv_abi", "diluvium", "summary", "upgrading"}
# Not a scalar: a mapping of profile name -> the connectors that profile
# carries. This is the field that makes a version number honest. A release
# whose connector set changed is not a patch release, whatever the digits
# say, because `requires.connectors` is checked against it by name.
MAPPINGS = {"connectors"}
KNOWN = SCALARS | MAPPINGS | {k for k, _ in SECTIONS}


def load():
    with open(SOURCE) as f:
        return yaml.safe_load(f)


def validate(doc):
    """-> list of problems, empty when the file is sound."""
    bad = []
    if doc.get("schema") != 1:
        bad.append("schema must be 1")
    releases = doc.get("releases") or []
    if not releases:
        bad.append("no releases")

    seen_v, seen_t, latest = set(), set(), []
    for r in releases:
        v = r.get("version", "<unnamed>")
        where = "release %s" % v

        for key in r:
            if key not in KNOWN:
                bad.append("%s: unknown key %r" % (where, key))
        for key in ("version", "tag", "status", "stable", "mirror", "summary"):
            if r.get(key) in (None, ""):
                bad.append("%s: missing %s" % (where, key))

        if v in seen_v:
            bad.append("%s: duplicate version" % where)
        seen_v.add(v)

        tag = r.get("tag")
        if tag:
            if tag in seen_t:
                bad.append("%s: duplicate tag %s" % (where, tag))
            seen_t.add(tag)
            # DRT has no upstream tags to collide with, so `v{version}`
            # would be derivable -- but it stays explicit anyway, because a
            # derived tag is one fewer place the file can be caught lying.
            if tag != "v" + str(v):
                bad.append("%s: tag %r should be %r" % (where, tag, "v" + str(v)))

        status = r.get("status")
        if status not in STATUSES:
            bad.append("%s: status %r not one of %s"
                       % (where, status, ", ".join(sorted(STATUSES))))

        date = r.get("date")
        if status == "unreleased":
            if date:
                bad.append("%s: unreleased but carries a date" % where)
        elif not date:
            bad.append("%s: %s but has no date" % (where, status))
        elif not re.fullmatch(r"\d{4}-\d{2}-\d{2}", str(date)):
            bad.append("%s: date %r is not ISO yyyy-mm-dd" % (where, date))

        if r.get("mirror") and status != "released":
            bad.append("%s: mirror: true but status is %r -- the mirror can "
                       "only carry a published release" % (where, status))

        if r.get("latest"):
            latest.append(r)

        # SCALARS is declared above and was never enforced, which is how a
        # 'upgrading:' written as '- |' instead of '|' -- copying the style of
        # the 'security:' block right below it -- passed validate and then
        # crashed render in CI. The two halves of this file disagreed about a
        # type and only one of them said so.
        conns = r.get("connectors")
        if conns is not None:
            if not isinstance(conns, dict):
                bad.append("%s: connectors must be a mapping of profile -> "
                           "list" % where)
            else:
                for prof, names in conns.items():
                    if not isinstance(names, list) or not all(
                            isinstance(n, str) for n in names):
                        bad.append("%s: connectors.%s must be a list of "
                                   "strings" % (where, prof))

        for key in sorted(SCALARS):
            val = r.get(key)
            # By what it is not, rather than by an allowlist: YAML resolves a
            # bare 'date: 2026-01-01' to a datetime.date, and an allowlist of
            # scalar types would have to name every tag the resolver knows.
            if not isinstance(val, (list, tuple, dict, set)):
                continue
            bad.append("%s: %s must be a single value, not a %s -- a block "
                       "scalar is '%s: |', not '%s:' followed by '- |'"
                       % (where, key, type(val).__name__, key, key))

        for key, _ in SECTIONS:
            items = r.get(key)
            if items is None:
                continue
            if not isinstance(items, list):
                bad.append("%s: %s must be a list" % (where, key))
                continue
            for i, item in enumerate(items):
                if not isinstance(item, str) or not item.strip():
                    bad.append("%s: %s[%d] must be a non-empty string"
                               % (where, key, i))

    if len(latest) != 1:
        bad.append("exactly one release must carry 'latest: true' (found %d)"
                   % len(latest))
    else:
        r = latest[0]
        for key in ("stable", "mirror"):
            if not r.get(key):
                bad.append("release %s is latest but %s is not true"
                           % (r.get("version"), key))
        if r.get("status") != "released":
            bad.append("release %s is latest but is not released"
                       % r.get("version"))
    return bad


def heading(r):
    date = r.get("date") or "unreleased"
    text = "## [%s] - %s" % (r["version"], date)
    marks = []
    if not r.get("stable"):
        marks.append("prerelease")
    if r.get("status") == "tagged":
        marks.append("tagged, not published")
    if marks:
        text += " (%s)" % ", ".join(marks)
    return text


def bullets(items):
    """A bullet may be multiline; its first line is the headline, and the
    rest is indented under it so Markdown keeps it inside the item."""
    out = []
    for item in items:
        lines = item.rstrip("\n").split("\n")
        out.append("- " + lines[0])
        for line in lines[1:]:
            out.append(("  " + line).rstrip())
    return out


def render_release(r):
    out = [heading(r), ""]
    meta = []
    if r.get("tag"):
        meta.append("`%s`" % r["tag"])
    if r.get("dv_abi") is not None:
        meta.append("dv ABI %s" % r["dv_abi"])
    if r.get("diluvium"):
        meta.append("diluvium `%s`" % str(r["diluvium"])[:12])
    if meta:
        out += [" &middot; ".join(meta), ""]
    if r.get("summary"):
        out += [r["summary"].rstrip("\n"), ""]
    if r.get("connectors"):
        out += ["### Connectors", ""]
        for prof in sorted(r["connectors"]):
            out.append("- `%s`: %s" % (prof, ", ".join(
                "`%s`" % n for n in r["connectors"][prof])))
        out.append("")
    for key, title in SECTIONS:
        if r.get(key):
            out += ["### " + title, ""] + bullets(r[key]) + [""]
    if r.get("upgrading"):
        out += ["### Upgrading", "", r["upgrading"].rstrip("\n"), ""]
    return "\n".join(out).rstrip("\n") + "\n"


def render_md(doc, tag=None):
    if tag:
        for r in doc["releases"]:
            if r["tag"] == tag:
                return render_release(r)
        sys.exit("changelog.py: no release with tag %r" % tag)
    head = (
        "# Changelog\n\n"
        "All notable changes to DRT are recorded here.\n\n"
        "Generated from `CHANGELOG.yaml`, which is the source of truth --\n"
        "edit that file, then run `script/changelog.py generate`.\n\n"
        "The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).\n\n"
        "DRT versions independently of diluvium and *records* the coupling\n"
        "rather than encoding it: each entry names the dv ABI it speaks and\n"
        "the diluvium revision it embeds, the same facts `BUILDINFO.txt`\n"
        "carries in the release. See `doc/Release.md`.\n"
    )
    return head + "\n" + "\n\n".join(render_release(r) for r in doc["releases"])


def render_json(doc):
    """What the mirror consumes. Rendered Markdown travels with each entry
    so the mirror needs no renderer of its own."""
    out = {
        "schema": doc["schema"],
        "repo": doc["repo"],
        "latest": next((r["tag"] for r in doc["releases"] if r.get("latest")),
                       None),
        "mirror_tags": [r["tag"] for r in doc["releases"] if r.get("mirror")],
        "releases": [],
    }
    for r in doc["releases"]:
        entry = {k: r.get(k) for k in
                 ("version", "tag", "date", "status", "stable", "mirror",
                  "dv_abi", "diluvium", "connectors", "summary",
                  "upgrading")}
        # PyYAML gives an unquoted yyyy-mm-dd back as a datetime.date; the
        # mirror wants a plain ISO string.
        entry["date"] = str(r["date"]) if r.get("date") else None
        entry["latest"] = bool(r.get("latest"))
        entry["sections"] = {k: r[k] for k, _ in SECTIONS if r.get(k)}
        entry["notes_md"] = render_release(r)
        out["releases"].append(entry)
    return json.dumps(out, indent=2) + "\n"


def read(path):
    with open(os.path.join(ROOT, path)) as f:
        return f.read()


def consistency(doc):
    """The newest entry describes the tree as it stands, so the tree has to
    agree with it. Version numbers live in a workspace field and a dozen
    path dependencies here and drift quietly; this is what makes that loud.
    -> list of problems."""
    bad = []
    r = doc["releases"][0]
    version = r["version"]
    where = "newest entry (%s)" % version

    # A release candidate is `X.Y.ZrcN` -- no hyphen, matching the other
    # repositories' candidates -- and the crates it is built from
    # stay at `X.Y.Z`: the tag is the release's identity, the manifest
    # version is the code's, and a candidate is the same code as the
    # release it is a candidate for. Bumping a dozen path dependencies to
    # `rc1` and back again would be churn that proves nothing, so the
    # comparisons below are made against the base version.
    m = re.fullmatch(r"(\d+\.\d+\.\d+)(?:rc\d+)?", str(version))
    if not m:
        bad.append("%s: version is not X.Y.Z or X.Y.ZrcN" % where)
        return bad
    version = m.group(1)

    # Every `version = "..."` in a workspace manifest, not just the first:
    # the path dependencies carry it too, and cargo will not build if they
    # disagree -- but it *will* build if they agree with each other and
    # disagree with the changelog, which is the drift this catches.
    for path in ("Cargo.toml", "crates/drt-web/Cargo.toml"):
        try:
            text = read(path)
        except OSError as e:
            bad.append("%s: cannot read (%s)" % (path, e))
            continue
        found = set(re.findall(r'version\s*=\s*"(\d+\.\d+\.\d+)"', text))
        wrong = sorted(v for v in found if v != version)
        if wrong:
            bad.append("%s carries version %s but %s says %r"
                       % (path, ", ".join(repr(w) for w in wrong), where,
                          version))

    # The embedded diluvium revision is a compatibility fact the release
    # publishes, so a changelog claiming one revision while Cargo.lock pins
    # another would put a wrong number in BUILDINFO's neighbour.
    if r.get("diluvium"):
        try:
            lock = read("Cargo.lock")
        except OSError as e:
            bad.append("Cargo.lock: cannot read (%s)" % e)
        else:
            m = re.search(
                r'name = "diluvium"\nversion = "[^"]*"\n'
                r'source = "git\+[^#]*#([0-9a-f]+)"', lock)
            if not m:
                bad.append("Cargo.lock: no git revision pinned for diluvium")
            elif not m.group(1).startswith(str(r["diluvium"])[:12]):
                bad.append("Cargo.lock pins diluvium %s but %s says %s"
                           % (m.group(1)[:12], where,
                              str(r["diluvium"])[:12]))
    return bad


def release_check(doc, tag, publishing):
    """Gate a release on its changelog entry. -> (problems, outputs)."""
    entry = next((r for r in doc["releases"] if r["tag"] == tag), None)
    if entry is None:
        return (["no entry in CHANGELOG.yaml for tag %r -- add one before "
                 "releasing it" % tag], {})
    bad = []
    if publishing:
        if entry["status"] != "released":
            bad.append(
                "%s is still status: %s. Before publishing, edit "
                "CHANGELOG.yaml: set status: released and a date, move "
                "latest: true onto it, set mirror: true, then re-run "
                "script/changelog.py generate and commit."
                % (tag, entry["status"]))
        if not entry.get("date"):
            bad.append("%s has no date" % tag)
    return bad, {"prerelease": "false" if entry.get("stable") else "true",
                 "version": entry["version"]}


def main():
    ap = argparse.ArgumentParser(add_help=False)
    ap.add_argument("command", choices=["validate", "render", "mirror-tags",
                                        "latest", "generate", "check",
                                        "consistency", "release-check"])
    ap.add_argument("format", nargs="?", choices=["md", "json"])
    ap.add_argument("--tag")
    ap.add_argument("--publish", action="store_true")
    ap.add_argument("-h", "--help", action="store_true")
    args = ap.parse_args()
    if args.help:
        print(__doc__)
        return 0

    doc = load()
    problems = validate(doc)
    if problems:
        for p in problems:
            print("CHANGELOG.yaml: " + p, file=sys.stderr)
        return 1

    if args.command == "validate":
        print("OK: %d releases, latest=%s, %d mirrored"
              % (len(doc["releases"]),
                 next(r["tag"] for r in doc["releases"] if r.get("latest")),
                 sum(1 for r in doc["releases"] if r.get("mirror"))))
    elif args.command == "render":
        if args.format == "json":
            sys.stdout.write(render_json(doc))
        else:
            sys.stdout.write(render_md(doc, args.tag))
    elif args.command == "mirror-tags":
        for r in doc["releases"]:
            if r.get("mirror"):
                print(r["tag"])
    elif args.command == "latest":
        print(next(r["tag"] for r in doc["releases"] if r.get("latest")))
    elif args.command == "consistency":
        problems = consistency(doc)
        if problems:
            for p in problems:
                print("inconsistent: " + p, file=sys.stderr)
            return 1
        print("OK: the workspace manifests and the diluvium pin agree "
              "with %s" % doc["releases"][0]["version"])
    elif args.command == "release-check":
        if not args.tag:
            sys.exit("changelog.py: release-check needs --tag")
        problems, out = release_check(doc, args.tag, args.publish)
        if problems:
            for p in problems:
                print("release-check: " + p, file=sys.stderr)
            return 1
        for k, v in out.items():
            print("%s=%s" % (k, v))
    elif args.command in ("generate", "check"):
        want = {MD: render_md(doc), JSON: render_json(doc)}
        stale = []
        for path, text in want.items():
            name = os.path.relpath(path, ROOT)
            if args.command == "generate":
                with open(path, "w") as f:
                    f.write(text)
                print("wrote %s" % name)
            else:
                try:
                    with open(path) as f:
                        current = f.read()
                except OSError:
                    current = None
                if current != text:
                    stale.append(name)
        if stale:
            print("stale, re-run 'script/changelog.py generate': %s"
                  % ", ".join(sorted(stale)), file=sys.stderr)
            return 1
        if args.command == "check":
            print("OK: CHANGELOG.md and changelog.json match CHANGELOG.yaml")
    return 0


if __name__ == "__main__":
    sys.exit(main())
