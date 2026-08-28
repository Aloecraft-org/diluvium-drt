# Releasing DRT

The strategy in one sentence: **DRT versions independently of diluvium,
records the coupling instead of encoding it, and lands on the same release
mirror as a sibling namespace** — so Lab, the deploy scripts and the
install one-liner all keep reading one host with one layout.

## Versioning: independent, with the coupling recorded

DRT tags its own `vX.Y.Z`. A DRT release does not name a diluvium release
— it *embeds* a diluvium revision (the git dependency in `Cargo.lock`) and
speaks one dv ABI version, and both facts are stamped into the release's
`BUILDINFO.txt`. "Which diluvium is inside" is something you read off the
artifact, never something you infer from a tag-naming convention. When a
DRT release happens to embed a published diluvium tag, the mirror's index
can say so; when it embeds a plain revision, that is equally fine and
equally recorded.

This is SPEC.md's version-first doctrine applied to distribution: the
compatibility fact travels with the bytes.

## Before you publish

The rehearsal is the gate, not a formality: **Actions → Release → Run
workflow** with `publish` off runs the tests and builds and smoke-tests
every platform, leaving the artifacts on the run. Only when that is green
does the same dispatch with `tag` set and `publish=true` touch the
Releases page. The publish job depends on tests and builds, so a failure
anywhere means no release rather than a partial one.

### wasm32 is not in the matrix yet, and that is the decision

`drt-web` is `crate-type = ["cdylib", "rlib"]`, so `cargo build -p drt-web
--target wasm32-unknown-unknown` does produce a `.wasm`. Checked what is
actually in it (2026-08-28): **one export, `memory`, and no callable
functions.** There is no wasm-bindgen export layer yet — that is task #31,
and it is the thing the Lab's Stage 3 waits on.

So the artifact would be inert. Two reasons that is worse than absent:

- A release artifact is a **compatibility surface**. Putting
  `drt_web.wasm` in releases.json now means a name consumers can resolve,
  for a file none of them can call.
- The day exports land, that file's contents change completely while its
  name does not. A consumer that fetched the earlier one has no way to
  tell them apart short of introspecting the export table.

**The trigger for adding it:** the export layer exists and the `.wasm`
exports the sixteen functions `doc/Browser.md` names (seventeen with
`handleOf`). At that point add `wasm32-unknown-unknown` to the build
matrix and `profile.web.exports` to BUILDINFO, so the same
"say-what-you-carry" rule that covers the native profiles covers this one.

What is in place meanwhile is the `wasm` job in `ci.yml`: drt-web compiles
for wasm32 on every push. That moves "the browser tier stopped building"
from "discovered when someone starts Stage 3" to "discovered the day it
broke", which is the part of this worth having early. The mirror half
needs no DRT change — `install.sh` already falls back to GitHub Releases,
so the Lab can consume from GitHub until a mirror namespace exists.

### Open: an unexplained SIGSEGV, twice, in two different binaries

The first v0.3.0 publish attempt died in `cargo test --workspace
--all-features` with **SIGSEGV in the `host_lua` test binary**, ~87 ms in,
before any of its 6 tests reported
([run 33157346043](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33157346043)).
It has not reproduced since, and it is unexplained. What was checked:

- The same command on the next commit: **passed**, as did both `ci` runs.
- Locally, on rustc 1.98.0 (CI's stable) *and* 1.94.1: ~2,600 runs of that
  exact test binary, 13 full `--all-features` suites — all clean.
- Under valgrind, in parallel: **0 errors from 0 contexts**. So not heap
  corruption.
- A deliberate stress of concurrent engine lifecycle — 64,800 instances
  across 24 threads, mixing budget exhaustion, faults, snapshots and
  `unsafe_stdlib` — clean. So *not* concurrent instance lifecycle, which
  was the leading theory: the test harness is the only place in the tree
  where several engines live in one process at once.
- Thread stack size from 128 KiB up: no change. So not stack overflow.
- The C core is **byte-identical to v0.2.0's**: the diluvium pin bump
  (f137b30) changed only `diluvium-sys/build.rs`, and the native compile
  flags it produces (`-O2 -std=c99 -DMAKE_LIB -fPIC -DLUA_USE_LINUX`) are
  the same ones the old script passed.

Then the decisive one, since local evidence was never going to settle a
crash that only ever happened on a runner: `.github/workflows/segv-probe.yml`
asks the machine it happened on, with core dumps armed
([run 33183183257](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33183183257)).

- **500 runs of `host_lua-a203f73e97f90f57` — the same binary hash that
  crashed — 0 failures, no core dumps.** Same runner image, same rustc,
  same build artifact, not merely a similar one.
- 3 more full `--all-features` suites on the runner: 0 failures.
- Locally, 43,200 concurrent `drt::start` deployments (the shape the cap6
  test contributes, which is richer than a bare engine): clean.

That does not *prove* infrastructure, because nothing can prove a negative
about a core dump that no longer exists. Nothing DRT ships drives instances
from more than one thread — `run`, `start` and `repl` are one instance per
thread, which is the dv.h contract — so the exposure, if it is real at all,
looks like the test harness and not a deployment.

#### Named at last: tokio runtime teardown (segv-probe run 6)

`segv-probe` [run 33194285525](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33194285525)
produced the first symbolized core of this hunt. The `stun` crash is:

```
#22 stun::a_pair_classifies_the_mapping_and_one_refuses_to  (tests/stun.rs:134)
#21 core::ptr::drop_glue<tokio::runtime::runtime::Runtime>
#20 core::ptr::drop_glue<tokio::runtime::blocking::pool::BlockingPool>
#19 tokio::runtime::blocking::pool::{impl#4}::drop        (pool.rs:284)
#18 tokio::runtime::blocking::pool::BlockingPool::shutdown (pool.rs:263)
#17 tokio::runtime::blocking::shutdown::Receiver::wait     (shutdown.rs:67)
```

while **thread 4**, a runtime worker, was in:

```
#8 tokio::runtime::park::Inner::unpark                     (park.rs:203)
#6 parking_lot::condvar::Condvar::notify_one               (condvar.rs:135)
#5 parking_lot::condvar::Condvar::notify_one_slow          (condvar.rs:172)
#0 core::sync::atomic::atomic_store  dst=0x7f7964008390    <-- faulted here
```

The main thread is inside `Runtime` drop, waiting on the blocking pool
(4 workers, `last_exited_thread: None`); a worker is concurrently storing
into the `Condvar` of a `park::Inner` that teardown has already freed.
**A use-after-free in tokio runtime shutdown** — not in DRT code, and not
in the C engine, which is where every earlier theory here pointed.

Versions: tokio **1.53.1**, which is the current release (`cargo update`
finds nothing newer), parking_lot 0.12.5, parking_lot_core 0.9.12.

Why these tests reach it: every `stun` probe resolves its server through
`lookup_host`, which is a `spawn_blocking`, so each test leaves idle
blocking workers parked; dropping the runtime then races their wakeup.

Fixed on our side by giving each test binary **one runtime in a `static`,
never dropped** (`tests/stun.rs`, `tests/relay.rs`) — a static is not
dropped at exit, so the teardown path never runs. The tests share one
runtime and get faster as a side effect.

**What this does NOT explain, and must not be claimed to:** the original
`host_lua` crash. That binary never constructs a tokio runtime (no
`tokio`, no `async`, no `Runtime` anywhere in it, and its cap6 deployment
configures neither relay nor stun). So this is either a second, distinct
crash, or the host_lua one is still unexplained. It stays open.

**Shipped-code exposure, worth a decision rather than a shrug:** the
bridges (`RelayBridge`, `StunBridge`) hand their runtime to a thread that
outlives the process, so they never drop one. The foreground verbs —
`drt relay`, `drt stun`, `drt tunnel` — *do* drop a runtime when they
return, which is this exact path. The blast radius is a clean shutdown
turning into a SIGSEGV: a supervised service would read that as a crash
rather than a stop.

#### The second occurrence, which changed the picture (ci run 10)

`ci` [run 33192177326](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33192177326)
died the same way — `signal: 11, SIGSEGV` — in **`tests/stun.rs`, a test
binary written that afternoon**, three tests in. That is the fact worth
keeping, because it retires several things said above:

- **It is not `host_lua`-specific.** Every theory framed around that one
  binary, this document's included, was over-fitted to a sample of one.
- It is not the cache, and not rebuild-then-run: probe rounds 2 and 3
  tested both and killed both (8 rebuild-then-run rounds, clean).
- The two binaries that have died have one thing in common: each runs a
  full `drt::start` deployment — a live engine — concurrently with other
  tests in the same process. That is the sharpest description available,
  and it is a description, not a diagnosis.

Still zero local reproductions: 200 runs of the `stun` binary after the
crash, 0 signal deaths. So the instruction stands — instrument the scene,
do not keep asking this machine. Cores are now armed in **`ci.yml` as well
as `release.yml`**, and the probe loops the `stun` binary rather than the
whole suite, which is two orders of magnitude more attempts per minute of
runner time.

What the same run *also* found, and what is fixed: two genuinely racy
assertions in those new stun tests (mine, not DRT's). `requests` is
counted before the reply goes out and `responses` after, so a snapshot can
legitimately land between them; both tests now wait for the settled state
instead of asserting on whichever snapshot arrived first. 3 failures in
150 runs before, 0 in 200 after.

The probe workflow is kept rather than deleted: if this recurs, one
dispatch turns a bare `signal: 11` into a backtrace. Treat a recurrence as
a finding for the diluvium session with both runs linked, not as a flake
to re-run past.

## The workflow

`.github/workflows/release.yml`, shaped like diluvium's on purpose:

- **Push a tag `v*`** → tests, builds every platform, publishes.
- **Actions → Release → Run workflow** → a full rehearsal by default:
  tests and all platforms, artifacts left on the run, nothing published
  until `publish=true`.

Artifacts use the mirror's flat naming, with `full` as the unprefixed
default — DRT installs as *the runtime*, and a runtime that cannot serve
a fetchpoint is the two-binary cliff again:

```
drt_linux_static_x86_64        # full: engine, connectors, listen, tunnel
drt_slim_linux_static_x86_64   # slim: the distribution-size profile
drt_darwin_arm64               drt_slim_darwin_arm64
drt_darwin_x86_64              drt_slim_darwin_x86_64
BUILDINFO.txt                  SHA256SUMS.txt
```

Linux aarch64 and Windows are next, not promised: `full` carries
`aws-lc-sys` through russh, and cross-compiling that is a thing to
rehearse (the workflow's dispatch mode exists for exactly this), not to
assume. The changelog-as-gate machinery diluvium's release carries is
worth adopting once DRT has releases worth gating; it is deliberately not
cargo-culted in on day one.

## The mirror (the server-side half)

One ask outside this repo: run the existing mirror generator a second
time, pointed at `Aloecraft-org/diluvium-drt`, into a sibling namespace —

```
https://diluvium.aloecraft.org/release/drt/<tag>/…
https://diluvium.aloecraft.org/release/drt/latest/…
https://diluvium.aloecraft.org/release/drt/releases.json
```

Same per-tag directories, same `latest/` stable path, same
`releases.json` shape (plus a `diluvium` field per release carrying the
embedded revision from `BUILDINFO.txt`). Lab and every deploy script then
learn one new path segment and nothing else.

## Building for wasm

`--no-default-features` carries the swarm and the capability layers without
the C core, and that is what a browser build starts from — there the
`Engine` bridges to a JS-hosted diluvium instance rather than linking one
(SPEC.md §4).

Building the `engine-diluvium` feature *for* a wasm target is the other
case, and it needs a wasi-sdk (>= 24) named by `WASI_SDK_PATH`, because the
C core must be compiled by a clang that can target wasm32. Without it
`diluvium-sys` refuses with an explanation. That refusal is deliberate and
worth knowing about: until recently the build *succeeded* and emitted a
host `.o`, so `cargo build --target wasm32-unknown-unknown` looked green and
only failed when something forced an actual link.

## Installing

`install.sh` at the repo root, served from the mirror:

```
curl -fsSL https://diluvium.aloecraft.org/release/drt/install.sh | sh
```

Mirror-first with a GitHub Releases fallback, SHA-256-verified when the
mirror's sums are reachable, `DRT_SLIM=1` / `DRT_VERSION=vX.Y.Z` /
`DRT_PREFIX=…` as the knobs.

**On DRT as the primary install candidate: yes, staged.** The argument
for: one binary that runs a script (`drt run`), serves a deployment
(`drt start`), reads the C host's own configs, and tunnels (`drt
tunnel`) covers everything a newcomer reaches for plus the runtime story,
while the `diluvium` interpreter remains the compiler/embedding artifact.
The staging: keep `/start` pointing at diluvium until discofetch runs on
DRT in production; flip it after — an install one-liner should hand out
the thing production has proven, and that proof is days away, not months.
