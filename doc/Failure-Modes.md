# Failure modes: where a crash can surface, how it is detected, what it costs

FMEA-style, and deliberately narrow: this covers the process-level crash
modes DRT is known to have, not a general risk register. Written because
"we found a segfault" is not an operational answer — the operational
answers are *which command*, *how often*, *what breaks*, *how you see it*,
and *what gets you running again*.

Each entry states its evidence. Where something is unmeasured it says so
rather than guessing a number.

---

## FM-1: tokio runtime teardown, use-after-free

**Mechanism.** Dropping a `tokio::runtime::Runtime` runs
`BlockingPool::shutdown`, which waits on the blocking workers. A worker
waking concurrently calls `park::Inner::unpark` →
`Condvar::notify_one_slow`, which stores into a `Condvar` that teardown
has already freed. Full backtrace in `Release.md`.

**Reached when** a runtime is dropped while a blocking worker is parked.
Every DRT path that resolves a hostname reaches this precondition, because
`lookup_host` is a `spawn_blocking` — so the pool always has a parked
worker to race.

**Versions.** tokio 1.53.1 (current release — there is no newer to take),
parking_lot 0.12.5, parking_lot_core 0.9.12. Not DRT code, not the C
engine.

**Observed rate.** One crash in ~400–500 runs of a test binary that builds
six runtimes per run under CI load. Zero in ~1,100 local runs (200
unpinned, 250 pinned to 2 cores, plus earlier sweeps). The rate on a
single-runtime command is unmeasured and is certainly lower; treat "rare
but real" as the claim, not a number.

### Where it could surface, before mitigation

| Site | Runtime dropped when | Exposure |
|---|---|---|
| `drt start` (relay and/or stun in the config) | **Never.** `RelayBridge`/`StunBridge` hand the runtime to a detached thread that never returns. | **None** |
| `drt tunnel <url>` — ProxyCommand, caller side | **Every session**, on normal successful teardown | **Highest** |
| `drt tunnel --park --to` — device side, on the fetchpoint | Only on unrecoverable error; the outer loop reconnects forever | Low |
| `drt tunnel --listen --to` | Only when the accept loop exits | Low |
| `drt relay` (standalone) | Only when `serve()` returns, i.e. the relay has already failed | Low, and already-failing |
| `drt stun` (standalone) | Only when the server returns, i.e. already failed | Low, and already-failing |

**The fetchpoint was never the exposure.** A rendezvous fetchpoint runs
`drt start`, whose bridges never drop a runtime; and the STUN server, being
one of those bridges, is not a source of this either. The exposure was the
*caller* side of `drt tunnel` — an operator's laptop, once per SSH session.

### Blast radius

Teardown happens **after** the work is done: the SSH session has already
ended, the bytes have already crossed, the relay has already metered them.
Nothing is half-written and no state is corrupted. The cost is an exit code,
not data.

- **Detection.** Exit status 139 (`128 + SIGSEGV`), `WIFSIGNALED`. Under
  `ssh -o ProxyCommand=...` the session is already closed, so ssh reports a
  failed ProxyCommand at teardown. Under a supervisor, a clean stop is
  misreported as a crash — which matters most for a service whose restart
  policy or alerting keys on that.
- **Recovery.** Retry. A new SSH session is a fresh process; the operation
  is idempotent because nothing partial survives.
- **Degraded mode.** Not required — there is no state to degrade.

### Mitigation, landed

The three foreground verbs (`relay`, `stun`, `tunnel`) now
`std::mem::forget` their runtime instead of dropping it. The process is
exiting; the OS reclaims everything drop would have, so leaking costs
nothing and removes the teardown path entirely. Test binaries
(`tests/stun.rs`, `tests/relay.rs`) hold one runtime in a `static`, which
is never dropped, for the same reason.

**Residual risk after mitigation: none known for this failure mode.** Every
site that dropped a runtime now leaks it or never had one.

---

## FM-2: the `host_lua` SIGSEGV — OPEN, unexplained

**Do not read FM-1 as covering this.** `host_lua` constructs no tokio
runtime — no `tokio`, no `async`, no `Runtime` anywhere in the binary, and
its cap6 deployment configures neither relay nor stun. FM-1's mechanism
cannot be its mechanism.

**Occurrences.** Twice, both in `cargo test --workspace --all-features` on
a GitHub runner: release runs 5 and 7 (2026-08-28). Never locally, in
several thousand runs across two rustc versions, under valgrind (0 errors),
and under a 64,800-instance concurrent-lifecycle stress.

**Why it matters more than FM-1 did.** The binary exercises the cap6 shape
— `drt::start::start` driving a real engine — which is the shape a
fetchpoint runs. If the cause is in that path rather than in the test
harness, the fetchpoint *is* exposed. That is not established; neither is
the opposite.

**Detection today.** Core dumps are armed in `ci.yml` and `release.yml`,
with a gdb backtrace step on failure, so the next occurrence yields a stack
rather than a bare `signal: 11`. That instrumentation is what turned FM-1
from six weeks of theories into one afternoon's answer.

**Probe result, 2026-08-28 19:29** ([run 33204062245](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33204062245)):
**1,600 runs of the `host_lua` binary on runners — 0 signal deaths.**

Read that narrowly. It loops the binary *alone*, which does reproduce its
own internal parallelism (6 tests across 4 threads) but not the machine
state of a full `cargo test --workspace --all-features`, which is what
both real occurrences happened under. So it lowers the estimate; it does
not close FM-2.

**Eliminated so far** (each tested, not reasoned): stale build cache;
rebuild-then-run; concurrent engine lifecycle; heap corruption; thread
stack size; a changed C core (byte-identical to v0.2.0's); CPU contention;
host_lua-specific framing (a second binary crashed too).

**Operational stance until it is named.** A crash here would be a hard
process death of the deployment, not a teardown nicety — so a fetchpoint
running `drt start` should be under a supervisor that restarts it
(`Restart=always`), and its restart count should be alerted on rather than
silently absorbed. That is ordinary practice regardless; this is a reason
not to skip it.

---

## What ego-proc does and does not cover

ego-proc is **not** a DRT dependency yet — `Cargo.toml` names it as work
that lands with the ForeignActor adapter, and nothing imports it today.

More to the point, it would not have covered either failure mode above even
once integrated. `SPEC.md` §12 is explicit: *"Relation to ego-proc's
lifecycle: none, deliberately. ego-proc's `LifecycleStatus`/`ControlSignal`
govern actors inside one process — the connector and service actors of §9.
The OS process's own lifecycle belongs to whatever started it."*

So ego-proc supervises **actors within a live process**: a connector dying
is restartable there, and a restart is semantically invisible to guests
because in-flight tokens get `status="error"`, which correct guests already
handle. A SIGSEGV kills the process and every actor in it — there is no
surviving supervisor to do the restarting. **Process-level crash recovery
is the init system's job**, by design, and that division is a choice DRT
made rather than a gap it left.
