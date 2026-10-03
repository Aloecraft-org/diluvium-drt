# DRT handoff

Written 2026-10-02, when the piecemeal Claude Code sessions ended and the
work moved to a Claude Code Project. The previous handoff, from
2026-08-30, is `doc/Handoff-2026-08-30.md`; nothing in it is current.

Read `.claude/CLAUDE.md` first for how the repository works. This file says
where the work stands and what is next.

---

## Where the code is

**The 0.8.0 work is on `claude/drt-dlua-ssh-tunnel-cdflop`, not on
`main`.** `main` is v0.7.0 plus documentation. The branch is about 55
commits ahead, contains `main`, and merges cleanly into it. CI is green
on the branch head.

The branch carries everything 0.8.0 is:

- `drt p2p` in four roles: call, `--listen`, `--park` (the native
  answerer of `doc/DRT-Signalling.md`), and `--match` (the reference
  signalling server), with `--relay` and `--fallback` as carriers.
- Pairing (DRT-Signalling §6.2), on both told sides: a parked `drt` and
  a page.
- `drt ssh`, the REPL's `:ssh`, an `ssh` listener for `drt start`, and
  one SSH server (`drt-sshd`) shared by native and page.
- The control endpoint: `drt ps` over the ssh listener, and `:ps`,
  `:status`, `:caps`, `:pause`, `:resume`, `:stop` from a REPL.
- Browser access with named services, Wisp half-close, `granted` after
  a service opens, and the REPL as a byte stream in a page.
- The release machinery: `NOTES.md` and the npm package as release
  assets, a BUILDINFO-against-changelog check at publish, and a `full`
  option that builds every target for a dev build.

**Merging the branch to `main` is the first step of cutting 0.8.0-rc.1**
(below). Until then `main`'s `release.yml` lacks the `full` input and
the new publish steps, because `dev-build.yml` dispatches the workflow
file on the default branch.

## Verified at handoff

- CI green on the branch at `67b112a`: format, clippy with warnings
  denied, both workspace feature sets, the examples gate, the Chromium
  suite, the SSH page gate, wasip2, native Windows, and fidelity.
- **v0.8.0-dev.22** is published from `67b112a` as a full dev build
  (Release run 93). It is the first build of every target since v0.7.0:
  Linux x86_64 and arm64 musl, macOS arm64 and x86_64, Windows full and
  slim with its smoke and examples gate on a Windows runner, wasip2,
  the web build, the browser access client and the SSH page. Its
  BUILDINFO has no `unknown` line and matches the 0.8.0 changelog entry.
- The 0.8.0 changelog entry was audited against every commit since
  v0.7.0, and carries Deprecated and Upgrading sections.

## Next: the release track

Cutting 0.8.0-rc.1. Steps marked **[human]** need a person: an account
that may push tags, or a decision.

1. **[human] Merge the branch into `main`.** By pull request, so the
   merge runs CI. The merge triggers a dev build of `main`; that is
   expected.
2. **Cut the entry.** In one commit on `main`:
   - Rename the newest `CHANGELOG.yaml` entry in place to
     `version: 0.8.0-rc.1`, `tag: v0.8.0-rc.1`, with `date`,
     `status: released`, `stable: false`, `mirror: true`. Do not add a
     second entry: only the newest may be `unreleased`. Leave
     `latest: true` on 0.7.0.
   - `.technoproj`: `pre` becomes `{"kind": "rc", "n": 1}`.
   - Every `version = "0.8.0"` stamp becomes `0.8.0-rc.1`: 22 in
     `Cargo.toml`, one in `crates/drt-web/Cargo.toml`. Then
     `cargo update -w`.
   - `python3 script/changelog.py generate`, then `validate`, `check`,
     `consistency`, and `release-check --tag v0.8.0-rc.1 --publish`,
     which must print `prerelease=true`.
   - `cargo test -p drt --test cli the_hard_coded_core_facts`.
3. **Rehearse.** Dispatch Release on `main` with `tag=v0.8.0-rc.1` and
   `publish` off. A release builds every target. dev.22 already proved
   the legs; this proves the changelog path with a real entry.
4. **[human] Tag from your own account:**
   `git tag v0.8.0-rc.1 && git push origin v0.8.0-rc.1`. A tag push runs
   the tagged commit's workflow. A dispatch-created tag failed once with
   a 403 (run 85, which left `v0.8.0-dev.15` a tag with no release). If
   the tag push's "Create release" fails, re-run that job in the same
   run; a fresh dispatch for an existing tag is refused.
5. **Verify the landing.** The release is a prerelease with 22 assets.
   `BUILDINFO.txt` names `dv_abi: 2` and `diluvium_version: 0.17.2`.
   After the mirror syncs, `latest-prerelease/` is v0.8.0-rc.1 and
   `latest/` is still v0.7.0. `install.sh` from the mirror prints
   `checked: sha256 ok`.

For 0.8.0 itself, `doc/Release.md` has the procedure: a new top entry
with `stable: true`, `latest: true` moved onto it, rc.1's `mirror`
cleared, `pre: null`, and the stamps back to `0.8.0`.

## Next: the feature track

The open list from the end of the 0.8.0 work, with where each item
stands on the branch. Ordered by what I would do first.

1. **Fold `drt netcheck`'s verdict into a p2p failure.** Built
   2026-10-03: a call with no carrier and no path ends with this side's
   verdict, measured against its `--stun` servers (`doc/P2P.md` §1).
2. **An answerer that parks several names at once.** Built
   2026-10-03: `--park` repeats, one host behind every name
   (`doc/P2P.md` §2.2).
3. **A seat token as the caller token.** Not built. DRT-Signalling
   leaves how a server is asked to the server; a room server would hand
   a seat token where `DRT-Caller-Token` goes.
4. **TURN as a last-resort candidate.** Not built: p2p drops relay
   candidates and uses no TURN. Today both ends behind symmetric NAT
   fail and say so unless the caller names `--relay` or `--fallback`.
   A TURN candidate, tried only when direct fails, keeps the rule that
   no third machine carries bytes unless asked, provided a flag still
   asks. It changes Browser Access v1's wire (`doc/BrowserAccess.md`
   §2.1 drops relay candidates), so it is a design note first.
5. **Plugins.** The `drt-plugin` crate works and `drt start` wires a
   `plugins` config block through it, behind the non-default `plugins`
   feature, with a check that a plugin cannot shadow a builtin family.
   What is missing is the design: `doc/Plugins.md` makes a plugin a
   connector backing and `doc/Peers.md` makes it a peer, and neither has
   been reconciled with the other. That decision comes before more code.
6. **Evaluating against a running deployment.** The control endpoint
   shows instances, queues and capabilities, and pauses and stops them.
   A REPL is still a fresh sealed instance per session; nothing
   evaluates against the instance `drt start` is running.
7. **The relay's "answer for me" (P2P §4.3) and TLS flags for
   `--match`.** Not built; `doc/P2P.md` line 6 says so. Low urgency
   until someone runs a relay in earnest.
8. **The launcher's `Root.attach`.** `drt-web` now exports the REPL as
   bytes in and out with `resize` (`bindings.rs`, `repl`), which is the
   shape native serves. `DrtEditor` still exists beside it. Whether the
   launcher switched over is launcher-side and not checked here.

Done on the branch, from the same list: the in-page REPL as bytes, the
control endpoint, Wisp half-close, the match server enforcing
`DRT-Accept` (the http listener hands programs the peer address),
`granted` after a service opens, and Windows: `p2p` builds and its tests
run natively in CI, and `full` cross-builds and passes the examples gate
on a Windows runner.

`doc/Ask-Discofetch-Reply-2.md` is the newest answer to Discofetch's
asks, written against this branch.

## Release QA findings not fixed

The 2026-10-02 release QA fixed what was wrong. These change behavior
rather than correct a fact, so they wait for a decision.

- **`drt tunnel`'s deprecation shim.** With `--local host:port` its
  suggested rewrite drops the host and is itself rejected. `drt ssh
  --help`, the REPL's `:ssh --help`, README's ProxyCommand text and
  examples 11 and 14 still present `tunnel` as current. Worth fixing
  before rc.1, since 0.8.0 deprecates the verb.
- **`netcheck` with nothing measured** prints a `relay` verdict and
  exits 1. Fixed 2026-10-03 by saying so: the help names the fallback,
  and `--json` carries `"measured": false`.
- **`drt run stdlib:wg`** gives the generic "parked" message rather than
  pointing at `drt start`, as the native stdlib programs do.
- **A broken sibling `.dlua`** fails every run in its directory, names
  a file the user did not run, and leaks the module loader's traceback.
- **`drt p2p --listen` and `--park` are silent** while gathering or
  retrying.
- **Coverage gaps:** no end-to-end test of the webrtc block's `call`
  command, of `--fallback` actually falling back, or of a successful
  `--pause`. The doc-test leg compiles nothing: every fence is `text`,
  `lua` or `js`.
- **Two fixed sleeps** in `tests/p2p.rs` and `tests/ssh_cli.rs` are the
  likeliest future flakes. None has fired.
- **Nits:** empty-argument errors with a bare colon, `tunnel` errors
  prefixed twice, `--show` not validating its argument, a missing record
  file reported as a DNS failure, no size cap on a program file, four
  wordings of "no root here", and example READMEs longer than a screen.

## Repository housekeeping

Remote branches other than `main` and the 0.8.0 branch. None was
deleted; deleting is the owner's call.

| Branch | State |
|---|---|
| `claude/beautiful-dijkstra-iyqxwo` | Fully merged into `main` (the v0.7.0 cut). Safe to delete. |
| `claude/zen-volta-cipkej` | One commit, the transport matrix doc, whose content the 0.8.0 branch carries. Safe to delete. |
| `claude/session-b-section-5-fn9180` | Three commits from 2026-09-18 not on any other branch by patch: B4 on the numeric core, a bench baseline recapture, a browser wait fix. Check before deleting. |
| `claude/drt-release-readiness-d57u1l` | v0.4.0-era. Three doc and changelog corrections not on other branches by patch; likely superseded. |
| `claude/drt-wasm-port-planning-4ua6qk` | v0.4.x-era. Eight commits on SSH in a page not on other branches by patch; the 0.8.0 branch rebuilt that work. Likely superseded. |

Also:

- `v0.8.0-dev.15` is a tag with no release (run 85's 403). Harmless.
- `main`'s `doc/ssh-transport-matrix.md` had unresolved conflict
  markers from 2026-09-29, and `doc/Platforms.md` still called the
  Windows binary the retired `windows` profile in two places. This
  handoff commit fixes both.

## Docs: current references and dated records

References, meant to track the code. The 2026-10-02 QA corrected the
stale facts it found in them; check a fact's date before leaning on it.

- `SPEC.md`, `GUARANTEES.md`, `README.md`, `examples/`
- `doc/P2P.md`, `doc/DRT-Signalling.md`, `doc/BrowserAccess.md`,
  `doc/ssh-transport-matrix.md`, `doc/SshInBrowser.md`, `doc/Browser.md`
- `doc/Release.md`, `doc/ALIGNMENT.md`, `doc/Relay.md`,
  `doc/WireGuard.md`, `doc/Wasm.md`, `doc/Modules.md`,
  `doc/Numeric.md`, `doc/Hostcall.md`, `doc/HostBaseline.md`,
  `doc/Failure-Modes.md`, `doc/Supervision.md`, `doc/Platforms.md`

Assessments and proposals, dated in their first line, not built or only
partly: `doc/Plugins.md`, `doc/Peers.md`, `doc/Wake-Policy.md`,
`doc/Aloelite.md`, `doc/Playwright.md`, `doc/Python.md`,
`doc/diluvium-numeric-spec.md`, `doc/diluvium-syntax-proposals.md`.

Records of a moment, read for history only: `doc/Plan-0.7.0.md`,
`doc/Plan-0.8.0.md` (its §7 status ends with what shipped beyond it),
`doc/Plan-2026-09.md`, `doc/0.7.0-ledger.md`, `doc/Next.md`,
`doc/Gap-Release.md`, `doc/Verification.md`, the `Ask-*-Reply*.md`
answers, the `*-Upstream.md` reports filed against other projects, and
`doc/Handoff-2026-08-30.md`.
