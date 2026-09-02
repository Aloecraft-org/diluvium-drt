# Changelog

All notable changes to DRT are recorded here.

Generated from `CHANGELOG.yaml`, which is the source of truth --
edit that file, then run `script/changelog.py generate`.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

DRT versions independently of diluvium and *records* the coupling
rather than encoding it: each entry names the dv ABI it speaks and
the diluvium revision it embeds, the same facts `BUILDINFO.txt`
carries in the release. See `doc/Release.md`.

## [0.4.0] - unreleased

`v0.4.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

The three §1 items from discofetch's 0.5.0 ask, which are the ones
where the runtime contradicted a documented guarantee.

Everything here is a promise DRT already made and did not keep. Two
of the three were found twice independently -- once by this
repository's own examples pass, once by discofetch reading the code
-- which is the argument for doing them before any feature.

The diluvium pin is unchanged from v0.4.0rc1 and the bump to build12
is still ahead. See known issues.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **`Connector::finish`**, and `Dispatcher::finish` over it. A
  connector holding state that outlives a hostcall says at teardown
  whether that state ended well; each string it returns is one thing
  that did not. Most connectors hold nothing and take the default.

  It exists for `sql` and is written so the next one does not need a
  new seam.
- **`examples/run-all.sh` skips what a build cannot run, instead of
  failing it.** `cargo build` with no flags is a **slim** binary, and
  eight examples need connectors or verbs slim does not carry -- so
  the obvious invocation failed eight examples at once, each with a
  diff whose real content was "this build does not carry that".

  `meta.json` gains `needs_build`, the runner reads the profile from
  `drt buildinfo`, and a mismatch is a named skip that is never
  counted as a pass -- the same rule `needs_network` already had. It
  also says the command to get the whole set, up front, rather than
  after eight diffs.
- **`drt buildinfo` reports the embedded diluvium revision.** It was
  in `BUILDINFO.txt` only -- a sidecar the release workflow writes by
  grepping `Cargo.lock` -- so a binary someone copied off a machine
  could not say which language core was inside it, and a package's
  `requires.diluvium` had nothing in the artifact to check against.
  `doc/Release.md`'s rule is that the compatibility fact travels with
  the bytes, and a fact in a file *beside* the bytes does not.

  Stamped at build time from the lockfile (`crates/drt/build.rs`),
  `unknown` for a build that does not pin diluvium by revision. The
  release workflow now reads it off the artifact like every other
  BUILDINFO field, with the lockfile grep kept as the fallback.

  A **revision**, deliberately, not a version. The core exposes no
  version string at runtime; and the distinctions that have actually
  mattered here -- FM-2 present or fixed, the budget escape open or
  closed -- separate `build12` from `build12p1`, which any semver
  comparison treats as equal because it ignores build metadata. A
  version field would be a field nothing could check.
- **`connectors/ssh` has tests.** It had none, which is how a
  connector that could not answer a single call shipped in v0.3.1 and
  went unnoticed for three days. Five now: scope validation by name,
  the unknown verb, and both halves of FM-3.

### Fixed

- **Budgets attenuate at spawn, in both the ways they did not.**
  `Budget::fits_within` and `InstanceConfig::check_attenuation` were
  written, correct, tested, and called from nowhere; `do_spawn` took
  the requested budget verbatim. A child could grant itself more
  instructions and more memory than its parent held. It is now
  refused by name -- `denied`, a reply rather than a fault, the same
  shape as a capability the parent does not hold.

  The second way is the one nobody had named, and it was the cheaper
  escape of the two because it took no intent at all: a child that
  stated *no* budget got `Budget::default()`, which is unlimited,
  under a bounded parent. `budget = nil` is what a spawn request
  looks like when nobody thought about it. An unstated bound now
  resolves to the parent's ceiling, which is what `fits_within`
  always claimed it meant, and a half-stated budget inherits the half
  it did not name.

  `08-spawn-and-hibernation` teaches "a child holds a subset of its
  parent's grants and nothing more". That sentence is now true of
  budgets as well as capabilities.
- **`sql` no longer discards an open transaction silently.** A
  program that opened a transaction and exited without committing got
  `ok` on every call, saw its own writes in-process, and lost them at
  exit with no error and exit 0.

  The connector now rolls back **explicitly**, names the database,
  and the process reports non-zero. SQLite would roll back anyway on
  teardown, so no outcome changes -- what changes is that the outcome
  no longer depends on a connection drop nobody here controls, and
  that the loss is *said*. A silent rollback and an accidental commit
  are both ways leaving it implicit can fail, and only one of them is
  recoverable.

  `drt start` does this as well as `drt run`, at both the places its
  swarm drains. That is the shape a fetchpoint actually runs in, and
  the one where an abandoned transaction is likeliest and least
  visible.

  Note what was **not** wrong, since the ask was written believing
  otherwise: `begin`, `commit` and `rollback` work, and a committed
  transaction survives. The module header saying "autocommit only"
  was the false part, and it is gone.
- **FM-3 is named, and both connectors that had it are covered.** A
  connector reaching `tokio::net` or `tokio::time` under
  `pollster::block_on` panics with "there is no reactor running" --
  `rest` in 0.4.0, `ssh` in v0.3.1, the same bug twice.

  Both fixes were already in; what was missing was any test that
  could fail. Every existing connector test was a `#[tokio::test]`,
  so all of them ran in the one configuration where the bug cannot
  appear -- `rest` had twenty-four. Both connectors now carry a plain
  `#[test]`, and both were confirmed to fail with the fix reverted
  rather than assumed to.

  The subtlety is recorded because the first attempt got it wrong:
  dialing a *closed* port does not reproduce this. The connection is
  refused immediately, the future never pends, and the timeout never
  arms its timer. The test needs something that actually waits.

### Known issues

- **A guest can hang the whole deployment (FM-4).** One line, needing
  no capability:

      while true do pcall(function() while true do end end) end

  Under `drt start` the deployment freezes -- not the child pinned
  and the rest running, but nothing running: no other instance steps,
  no listener is served. Measured, with the control case (the same
  child without the `pcall`) stopped by its budget in milliseconds.

  diluvium 5.5.1_build12p1 fixes the *accounting* half -- the hook
  stays armed, so an escaped instance can no longer report perfect
  health while running on -- and each catch still buys
  `DV_HOOK_STEP` instructions, so a loop of catches is still
  unbounded. Verified against the fixed build.

  DRT cannot close this from here: `dv.h` exposes no interrupt, the
  one hook slot is the budget's, and a CPU-bound guest never returns
  to the host for `dv_exceeded()` to be acted on. The fix is a
  core-file patch upstream. `doc/Failure-Modes.md` FM-4 has the
  operational answer, which is not `Restart=always` -- the process
  never dies -- but a liveness watchdog, and one process per tenant
  you do not trust.
- **The instruction budget is still escapable, and the pin is still
  pre-build12.** Both carried forward from v0.4.0rc1 unchanged; see
  that entry. The budget escape is upstream (`src/dv.c:219`);
  `build12p1` fixes the single-catch case and not the looping one.
- **`crypto/random` is not answered with no config**, and **wasm32 is
  not in the release matrix**, and **`drt ps` is a stub**. All
  unchanged from v0.4.0rc1.

### Upgrading

Two behaviour changes can turn a previously-zero exit non-zero, both
deliberately:

A spawn naming a budget larger than its parent's is now `denied`
rather than granted. If a supervisor relied on that, it was relying
on the bug -- but it will see a refusal it did not see before.

A program that leaves a SQL transaction open at exit now fails. It
was already losing the writes; it just was not told.


## [0.4.0rc1] - 2026-09-01 (prerelease)

`v0.4.0rc1` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

Outbound HTTP from a guest, a NAT diagnostic, and a set of examples
that found a bug in the first of those before anyone shipped it.

**A candidate, not the release.** It is cut so downstream work can
start against something with a tag rather than a branch name, and
it is cut *on the diluvium pin the examples were verified against*
-- `f137b30`, which is pre-build12. Taking the bump and shipping in
one move would mean publishing a set of examples nobody had run the
gate against on those bytes. 0.4.0 proper takes build12 and re-runs
the gate; that is the only difference planned between this and it.

Numbered 0.4.0 rather than 0.3.2 because the connector set changed:
`rest` is new, so `profile.full.connectors` in BUILDINFO is not what
v0.3.1's was, and a package declaring `requires.connectors` is checked
against that list by name. `netcheck` is a new verb on the same
argument.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **The `rest` connector: `host:rest/get` and `host:rest/post`.**
  The guest surface is diluvium's, from
  `plugins/rest/rest.plugin.json`, so a program written against
  `diluvium-host` runs unchanged — same calls, same shapes, and the
  same bounds read out of `rest_plugin.c` rather than picked.
  Redirects are not followed, because the C plugin does not follow
  them.

  Unlike the C host's out-of-process plugin, this takes a **scope**:
  an origin allowlist, checked against the URL *and* against the
  resolved address before connecting, since an allowed name that
  resolves into private space is the DNS rebinding shape. An allow
  entry may also carry `headers`, which the connector injects and the
  guest can neither set nor read — so an app calls an authenticated
  API without the program ever holding the credential — and
  `allow_headers`, which when present is the exhaustive set the guest
  may set on that origin.
- **`drt netcheck`.** One of four verdicts — `direct`, `v6-direct`,
  `punchable`, `relay` — with the measurements that produced it, per
  discofetch's `doc/NETCHECK-SPEC.md`. The verdict tree is a table
  rather than nested branches, because it is the part that will be
  wrong first when real home networks surprise us.

  The decisive measurement is the UDP mapping across two STUN
  servers, never the TCP one. A NAT can be endpoint-independent for
  TCP and symmetric for UDP, and reading the verdict off the TCP
  columns would be confidently wrong on exactly the networks where
  this matters most.

  Reflect and the prober — the edges' half — are not implemented
  here; the inbound test reports "not measured" until they exist.
- **`examples/`, a run-through that is also a gate.** Sixteen
  self-contained directories, each run from inside its own folder,
  each carrying an `expected.txt` captured from a real run, and
  `run-all.sh` to diff them. One needs the open internet and is
  skipped unless `--net` says otherwise; a skip is reported as a skip
  and never as a pass. The point of the gate is that this
  repository's own traps list says to re-run the examples when config
  parsing changes — now something does.
- `CHANGELOG.yaml` and `script/changelog.py`, ported from diluvium's, with the release body and the mirror's `changelog.json` generated from one source. CI fails if they drift.
- `doc/Editors.md` (how to get `.dlua` recognised, and why GitHub cannot match the editor extension) and `.gitattributes` mapping `.dlua` to Lua.
- `doc/Next.md`: the deferred work, sized against the code rather than estimated.
- `doc/Verification.md`: what the examples gate cannot reach -- the runs that need a real network, a second machine or a reachable sshd -- written for whoever has one.
- `doc/Ask-0.5.0-Reply.md`: the reply to discofetch's 0.5.0 ask, with the two `reported` findings reproduced and the three decisions DRT needs before starting.

### Changed

- **`drt run` no longer exits 0 for a program that escaped its
  instruction budget.** A guest can catch exhaustion with `pcall` and
  keep running (see known issues); until now `drt run` reported
  success for that, which made it the only place in DRT that hid it
  — `drt start` has always classified such a stop as `exceeded`.

  It is not enforcement and does not pretend to be: the program has
  already run. It is the difference between a budget that was escaped
  and a budget that was escaped silently, and a supervisor can only
  act on the second kind if something says so. A program that stays
  inside its budget is unaffected.

### Fixed

- **The `rest` connector panicked under `drt run`, and never shipped
  that way.** `drt start` drives connectors on a tokio runtime;
  `drt run` uses `pollster::block_on`, which carries no reactor, and
  every socket call needs one. A URL the allowlist *permitted* died
  with "there is no reactor running" and exit 101 — while every
  refusal worked, because refusals are decided before a connection is
  attempted. Found by writing `examples/05` against the connector, in
  the same release that introduced it. The connector now carries its
  own runtime for callers that have none, leaked rather than dropped
  for FM-1's reason.
- **`netcheck` advertised a flag the binary does not accept.** The
  evidence block printed `inbound  not tested (no --port given)`,
  and there is no `--port` in any build -- which also makes
  `direct`, the verdict that requires an inbound connect,
  unreachable from the CLI. It now reads `not measured (no inbound
  test in this build)`, which is true. The flag itself arrives with
  the reflect edges; `09-netcheck` already said so and the program
  now agrees with the example.
- `release.yml`'s publish job copied `install.sh` from one directory above the workspace, which does not exist. Under `set -eu` that failed the step and took the whole publish with it. Never caught because `publish` is the one job a rehearsal does not execute.

### Known issues

- **The embedded diluvium has the FM-2 data race.** This release pins
  `f137b30`, and diluvium 5.5.1_build12 (2026-09-01) names that
  revision and earlier as affected. DRT mitigates it by serialising
  instance creation behind a mutex in `drt-swarm`, so DRT's own
  exposure is closed; anything else embedding this revision is not.
  The pin bump is the real fix, is deliberately not taken in a
  candidate, and is the first thing 0.4.0 does. The mutex stays
  until it lands -- `crates/drt-swarm/src/engine.rs` carries the
  removal condition at the lock, so nobody has to rediscover it.
- **Budgets do not attenuate at spawn.** `Budget::fits_within` and
  `InstanceConfig::check_attenuation` (`crates/drt-config/src/lib.rs:69,114`)
  are written, correct and tested, and are called from nowhere else
  in the workspace. A child takes the budget it names, so it can
  grant itself more instructions and more memory than its parent
  holds. Capabilities attenuate; budgets do not.
  `08-spawn-and-hibernation` teaches "a child holds a subset of its
  parent's grants and nothing more", which is true of capabilities
  and false of budgets. Found independently by discofetch and by
  this repo's own examples pass.
- **A guest can switch its instruction budget off, permanently, in
  two lines.** `pcall` around a loop catches budget exhaustion as an
  ordinary Lua error; the budget never fires again for the life of
  the instance, and `drt run` still exits 0. Measured: exhaustion at
  ~250k steps under a 1,000,000 limit, then an unbounded loop still
  running when killed at 20 s.

  The cause is upstream, at the pin and in diluvium `main` alike:
  the instruction hook (`src/dv.c:219`) clears itself before raising
  -- "once is enough; the error is on its way" -- so a caught error
  leaves nothing armed. **build12 does not fix this**, and it cannot
  be closed from the host side: a CPU-bound loop never returns to
  the host, so there is no resume for `dv_exceeded()` to refuse.
  `doc/Ask-0.5.0-Reply.md` §1.2 is the brief.

  What this release does do is stop hiding it: `drt run` reports a
  non-zero exit for a program that escaped, so a supervisor sees an
  escape rather than a success. That is all a host can do from
  outside the VM.
- **SQL discards an open transaction at exit, silently.**
  `begin`/`commit`/`rollback` do work -- they pass through to SQLite
  on a held connection, and a committed row survives. But a program
  that opens a transaction and exits without committing gets `ok` on
  every call, sees its own write in-process, and loses it on exit
  with no error and no non-zero status. Correct SQLite behaviour on
  a dropped connection; the wrong contract for a durable tier. The
  connector's "autocommit only" header comment is the part that is
  wrong, not the code.

  Decided for 0.4.0, recorded in `doc/Ask-0.5.0-Reply.md` §3.1: the
  connector rolls the transaction back **explicitly**, names it, and
  the instance stops non-zero. The explicit rollback is the point --
  SQLite would roll back anyway on teardown, but leaving it implicit
  makes the outcome depend on a connection drop nobody here controls,
  and an accidental commit is the one failure that is not
  recoverable.
- **No `exec`.** DRT has no local process execution at all — no
  `std::process::Command` anywhere. `exec/run` answers denied and a
  config wiring `connectors.exec` is refused at load. `ssh/exec` runs
  commands on a *remote* host. See diluvium's `doc/DRT.md`.
- **`crypto/random` is not answered with no config**, although
  `doc/HostBaseline.md` names it one of three families every DRT host
  must answer or deny by name. `CryptoScopeType` requires a signing
  key even for the keyless calls, so an unscoped crypto family
  answers nothing. Two deliberate decisions in conflict; unresolved.
- **wasm32 is not in the release matrix.** `drt-web` now has a
  wasm-bindgen export layer and a browser test suite on a branch, but
  the connector/pump layer does not exist, so a program can run and be
  driven in a page and cannot reach `host.fs` or `host.time`.
- **`drt ps` is a stub**, and **the REPL has no line editor**.
- **Budgets bound the VM, not the deployment**: no wall-clock bound, no cumulative spawn bound. `doc/Next.md` sizes both.

### Upgrading

Nothing here removes or renames a surface. `rest` and `netcheck` are
additive and reach nothing unless a config wires them: `rest` answers
no call at all without a scope granting origins, and an empty
allowlist is a startup refusal rather than a runtime surprise.


## [0.3.1] - 2026-08-31

`v0.3.1` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

STUN, the C host's `access` spelling, and two named crashes.

Tagged the same day v0.3.0 shipped and published three days later,
after a repository ruleset on `v*` refused the workflow's App token
with a 403 and the release had to be created by hand.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **STUN.** `drt stun` serves RFC 5389 binding requests, a `stun`
  config block configures it, and inside `drt start` the same server
  reports its counters to the root program. Two servers on separate
  addresses is what makes mapping classification available at all:
  one vantage says what one vantage saw, and it takes two to know
  whether the mapping *changed*.
- `drt buildinfo --json`, and a `wasm` job in CI that compiles `drt-web` for wasm32 on every push.
- `install.sh` ships as a release asset, so the one-liner has a URL that exists without waiting on server-side work.
- `doc/HostBaseline.md`: the families any DRT host must provide or stub, measured against both hosts rather than reasoned about.
- `doc/Failure-Modes.md`, and `doc/FM-2-Upstream.md` — the brief for fixing FM-2 in diluvium.

### Changed

- **`access` is `read`, not `readonly`** — a compatibility fix, and
  the reason this release is not optional for anyone running the C
  host's configs. `dhost.c` accepts exactly `"read"` or
  `"readwrite"`; DRT accepted `"readonly"` and *refused* `"read"`, so
  an existing discofetch config failed DRT's parse and crash-looped
  under the deploy chain check.
- `BUILDINFO.txt` now says what a binary carries: `dv_abi` (promised since v0.1.0 and never kept) and the connector set per profile, both read out of the artifact by `drt buildinfo` rather than guessed in YAML.
- The README leads with a download that works today. The documented mirror URL 404s — the mirror never grew the `drt` namespace — and a doc that promises a 404 is how that went unnoticed.
- `install.sh` verifies both sources; a mismatch refuses, a missing sums file warns. `DRT_MIRROR` takes `file://` URLs, which is the air-gapped install with no new code.
- `examples/hello.dlua` rewritten against `host.*`, printing byte-identical output to the fifteen-line queue-boilerplate version it replaces. The old one is kept as `examples/by-hand.dlua`.

### Fixed

- **FM-1: a use-after-free in tokio runtime teardown.** `drt relay`,
  `drt stun` and `drt tunnel` dropped a `Runtime` on the way out,
  which races a parked blocking worker's wakeup into a freed
  `Condvar` in tokio 1.53.1. They leak it now.
- **FM-2: a data race in diluvium's continuation registries.**
  `diluvium_shim_addcont` appends to a process-global array with no
  synchronisation, so two threads calling `dv_new` at once can leave
  a slot whose name is NULL and the next scan segfaults in `strcmp`.
  DRT now serialises instance creation behind a mutex. The real fix
  is upstream and is not in this release; DRT's shipped exposure was
  nil either way, since `run`, `start` and `repl` create instances
  only on the drive-loop thread.
- **A relay-only deployment burned a whole core.** With no HTTP listener the drive loop's idle sleep became a spin — 99.7% CPU, forever, on exactly the rendezvous config `examples/rendezvous` documents. 3.0% now.
- The shipped `examples/deployment.json` did not load: the `access` fix above missed the one config file a new user copies. Found by running it rather than reading it.
- Linux non-x86_64 refuses by name in `install.sh` instead of downloading the x86_64 static binary and failing the `--version` guard.
- `doc/Browser.md`'s export table listed fifteen functions and omitted `release`.

### Known issues

- **The embedded diluvium has the FM-2 data race, and this release is
  published.** v0.3.1 pins `f137b30`; diluvium 5.5.1_build12 names that
  revision and earlier as affected. A host that creates instances on
  more than one thread can die in `strcmp` in the first microseconds of
  a fresh process.

  DRT's own exposure is nil in this release for the reason
  `doc/Failure-Modes.md` gives — `run`, `start` and `repl` create
  instances only on the drive-loop thread — so this affected DRT's test
  harness rather than any deployment. Recorded against the version it
  affects rather than only against the one that fixes it.

### Upgrading

**If you run configs written for the C host, this is the release that
parses them.** v0.3.0 refused `access: "read"` and accepted
`"readonly"`, which is backwards. A config edited to say `"readonly"`
as a workaround must be changed back.


## [0.3.0] - 2026-08-28

`v0.3.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

The rendezvous relay, end to end: a real `ssh` session over WSS
through `drt tunnel`, 37 bytes metered, rehearsed against the release
artifact.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- `drt relay`: the rendezvous relay -- parked WSS legs paired by label and spliced, with presence, metering and arbitration reported to the root program.
- `drt tunnel`: SSH over WSS as a dumb pipe, in three shapes -- the OpenSSH `ProxyCommand` contract over stdio, a WS→TCP listener, and the device side of the relay.

### Known issues

- `access` is spelled `readonly` here and refuses `read`, which is backwards from the C host. A config written for `dhost.c` does not parse. Fixed in 0.4.0.
- A relay-only deployment (no HTTP listener) spins a core at 99.7% forever. Fixed in 0.4.0.
- `drt relay`, `drt stun` and `drt tunnel` can SIGSEGV on clean shutdown (FM-1). Fixed in 0.4.0.


## [0.2.0] - 2026-08-27

`v0.2.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

Park mode: the device corner of the triangle.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`


## [0.1.0] - 2026-08-27

`v0.1.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

The first release: DRT as a static binary that runs sandboxed Lua
under a capability model and serves fetchpoints.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Known issues

- `BUILDINFO.txt`'s header promised a `dv_abi` field it did not emit. Kept from v0.1.0 in 0.4.0.
