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

## FM-2: a data race in diluvium's continuation registries — NAMED, mitigated here, fixed upstream in build12, pin not yet moved

**Do not read FM-1 as covering this.** `host_lua` constructs no tokio
runtime — no `tokio`, no `async`, no `Runtime` anywhere in the binary, and
its cap6 deployment configures neither relay nor stun. FM-1's mechanism is
not its mechanism, and the two crashes were never the same bug.

**Named 2026-08-31**, from the first symbolized core this hunt produced:
`ci` [run 33444175875](https://github.com/Aloecraft-org/diluvium-drt/actions/runs/33444175875).

```
Program terminated with signal SIGSEGV, Segmentation fault.
#0  __strcmp_evex ()
#1  diluvium_shim_addcont ()
#2  diluvium_openlibs ()
#3  dv_new ()
#4  diluvium::Instance::fresh (src/lib.rs:400)
...
#10 host_lua::a_relay_block_refuses_a_half_configured_label (host_lua.rs:217)
```

Two *other* threads were inside `dv_new` → `diluvium_openlibs` at the same
moment. That is the finding: three threads constructing instances at once.

**The mechanism.** `diluvium_shim_addcont` (`src/dshim.c:750` at pin
`f137b30`) appends to a process-global array — `dshim_conts`,
`dshim_ncont` — with no mutex, no atomic and no once-guard, and
`diluvium_openlibs` calls it on every `dv_new`. Two threads can claim the
same slot index, leaving a slot whose `name` is still `NULL`; the next
scan calls `strcmp(NULL, ...)` and the process dies. `src/dsnap.c:1312`
carries a second copy of the same function over a second array. Full
write-up, with the interleaving and the fix options, in
[`FM-2-Upstream.md`](FM-2-Upstream.md).

**Fixed upstream 2026-09-01**, in diluvium 5.5.1_build12: `src/dsync.h`
guards both registries, and that release names `f137b30` and earlier as
affected. DRT has not taken the bump — v0.4.0-rc.1 pins `f137b30` on
purpose, because the examples gate was captured against it — so for a
binary built from this tree the mitigation below is still what closes
FM-2, not a leftover.

**Why it never reproduced, which is the part worth keeping.** `addcont`
writes only when a name is not already present, so once every name is
registered the array is read-only for the life of the process. The window
is the first few microseconds of the first concurrent `dv_new` calls in a
fresh process, and then it closes permanently. Hammering a running process
samples that window exactly once. That is why 1,600 probe runs, ~2,600
local runs and a 64,800-instance stress all came back clean and all proved
nothing — and why the original crash landed ~87 ms in, before any test
reported, which is process start.

It also explains the clean valgrind run: the default tool is memcheck,
which does not detect data races. That needed helgrind or ThreadSanitizer.

**Shipped exposure: none.** `drt run`, `drt start` and `drt repl` create
instances only on the drive-loop thread — `listen.rs`'s per-connection
threads push `Ingress` over a channel and construct nothing, and spawns go
through `&mut Swarm`. DRT was never violating `dv.h`'s "one instance, one
thread" contract, and could not have: the contract is per *instance*, the
registries are per *process*, and nothing in the header covers them. The
two binaries that died were test harnesses, which are the only place in
this tree that calls `dv_new` from several threads at once.

**Mitigation, landed.** `crates/drt-swarm/src/engine.rs` serialises
instance *creation* behind a `Mutex`. Creation is rare and cheap next to
running a program, and the lock covers only creation — instances still run
concurrently, one per thread. This closes the cold-start window completely
for DRT. It is a mitigation in one host, not the fix; remove it when the
diluvium pin carries the upstream repair — `grep -A2 'name = "diluvium"'
Cargo.lock` showing build12 or later is the whole condition, and the
comment at the lock's use site says so too.

**Fixed upstream, not yet pinned here.** build12 carries the repair; this
tree does not carry build12. So the mitigation is still the thing holding,
and any *other* host embedding `f137b30` — drt-web included, once it hosts
more than one instance — has the bug unmitigated.

**Operational stance.** Unchanged and still worth doing: a fetchpoint
running `drt start` belongs under a supervisor that restarts it
(`Restart=always`), with the restart count alerted on rather than silently
absorbed.

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
