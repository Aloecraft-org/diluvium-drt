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

Outbound HTTP from a guest, a NAT diagnostic, and a set of examples
that found a bug in the first of those before anyone shipped it.

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
- **`examples/`, a run-through that is also a gate.** Seven
  self-contained directories, each run from inside its own folder,
  each carrying an `expected.txt` captured from a real run, and
  `run-all.sh` to diff them. The point of the gate is that this
  repository's own traps list says to re-run the examples when config
  parsing changes — now something does.
- `CHANGELOG.yaml` and `script/changelog.py`, ported from diluvium's, with the release body and the mirror's `changelog.json` generated from one source. CI fails if they drift.
- `doc/Editors.md` (how to get `.dlua` recognised, and why GitHub cannot match the editor extension) and `.gitattributes` mapping `.dlua` to Lua.
- `doc/Next.md`: the deferred work, sized against the code rather than estimated.

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
- `release.yml`'s publish job copied `install.sh` from one directory above the workspace, which does not exist. Under `set -eu` that failed the step and took the whole publish with it. Never caught because `publish` is the one job a rehearsal does not execute.

### Known issues

- **The embedded diluvium has the FM-2 data race.** This release pins
  `f137b30`, and diluvium 5.5.1_build12 (2026-09-01) names that
  revision and earlier as affected. DRT mitigates it by serialising
  instance creation behind a mutex in `drt-swarm`, so DRT's own
  exposure is closed; anything else embedding this revision is not.
  The pin bump is the real fix and has not been taken yet.
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
