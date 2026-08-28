# The browser tier: the contract between `drt-web` and its JS host

SPEC.md §4 designates the Lab's JS host as DRT's browser implementation.
This file is the interface between them, and DRT defines it for the same
reason DRT defines the relay's wire format: whoever owns the trait writes
the contract, and the other side consumes it.

**Status:** `crates/drt-web` implements the Rust half — the engine, the
host, and the bridge — proven natively by a mock bridge that drives a real
`Swarm` (`crates/drt-web/tests/bridge.rs`). The wasm-bindgen export layer
is the remaining piece. The JS half can be built against this now.

**And the deadline stops applying.** This path drops
`diluvium_swarm_wasi.wasm` entirely: the browser goes from loading
*kernel + swarm-module* to *kernel + drt-web*. So the Lab is not racing
`dvs.c`'s deletion, it is exiting the dependency — worth stating plainly,
because "beat the clock" and "the clock no longer applies" call for
different amounts of hurry.

## The shape: two directions at one boundary

`drt-web` is a `cdylib` that carries two Rust-side halves of one
wasm-bindgen boundary:

```
  JS  ──── exports ────>  drt-web  ────  drt-swarm  (the dvs.c port)
      <─── imports ─────       (Engine impl calls back out)
```

- **Exports** are the swarm's operations, for the panel and for app code.
- **Imports** are the `Engine`/`Instance` operations plus `drive`, which
  JS implements over a diluvium instance it already knows how to load.

Two wasm modules cannot call each other directly in a browser, so JS is
necessarily the host in the middle. That is not a workaround; it is why
SPEC.md §4 names the JS-host pattern as the browser fallback.

**No wasi-sdk is needed for this build.** `drt-web` depends on `drt-swarm`
with `--no-default-features` — the traits without the C core — so nothing
compiles C for wasm. (`WASI_SDK_PATH` is only for linking the C core into a
wasm artifact, which the browser tier deliberately does not do; see
doc/Release.md.)

## Exports: what JS may call

Clean wasm-bindgen types — **not** raw pointers. This is a deliberate
departure from `dvs_*`, whose signatures hand out pointers into another
language's structs. Reproducing that in Rust to satisfy one consumer would
make the API unsafe for every other consumer, and the CDN audience (people
building p2p web apps) must never touch a pointer. `swarm.js`'s
`swarmCapable(exports)` is already the seam where a second backend is
recognised; a `drtCapable` beside it is the natural migration, rather than
DRT impersonating a C ABI.

Fourteen of `REQUIRED_SWARM`'s sixteen map directly:

| `swarm.js` | `drt-web` export | note |
|---|---|---|
| `dvsjs_new` | `Swarm.new(maxInstances, spawnsPerStep)` | constructor |
| `dvs_free` | `Swarm.free()` | explicit; wasm has no GC hook |
| `dvs_root` | `root(code, caps, budget)` | |
| `dvs_step` | `step()` → alive count | |
| `dvs_alive` | `alive()` | |
| `dvs_instance` | `ids()` | roster, not a pointer |
| `dvs_parent` | `parent(id)` | |
| `dvs_kill` | `kill(id)` | |
| `dvs_push` | `push(id, queue, msgpackBytes)` | |
| `dvs_budget` | `budget(id)` | |
| `dvs_caps` | `caps(id)` | |
| `dvs_holds` | `holds(id, cap)` | capability gating stays reachable |
| `dvs_resident` | `resident(id)` | |
| `dvs_cached_size` | `cachedSize(id)` | |
| `dvs_last_error` | — | errors are thrown, not polled |
| `dvs_abi_version` | `abiVersion()` | plus the engine's own, via import |

DRT additionally exports what `dvs.c` never had a JS binding for:
`hibernate(id)`, `wake(id)`, `wakeOnMessage(id)`, `mayGrant(parent, cap)`,
`slotsAllocated()`, and the `allowHibernation`/`allowBytecode`/
`allowUnsafeStdlib`/`setHostIdentity` switches.

**Every export must be panic-safe.** A Rust panic crossing the boundary
mid-`step` corrupts swarm bookkeeping exactly as a JS exception unwinding a
wasm frame does — the hazard `swarm.js` already guards with `_faults`. Each
export wraps its body and converts a panic into a thrown JS error, never an
unwind.

## Imports: what JS must provide

One object, supplied at construction. Sixteen functions: the `Engine`
trait's three, `Instance`'s twelve, and `drive`.

```
  // Engine
  abiVersion()                      -> number
  load(source, name, budget, unsafeStdlib)      -> instanceHandle
  restore(snapshot, hostStamp, budget, unsafeStdlib) -> instanceHandle

  // Instance, per handle
  release(h)                        -> void
  queue(h, name)                    -> queueHandle | null
  queueInfo(h, q)                   -> {len, capacity, enabled, exported}
  push(h, q, bytes)                 -> 'accepted'|'droppedOldest'|'full'|'disabled'
  pop(h, q)                         -> bytes | null
  run(h)                            -> Step
  resume(h, firedQueue)             -> Step
  resumeTimeout(h)                  -> Step
  currentWait(h)                    -> {queues, timeoutMs, forSpace} | null
  usage(h)                          -> {instructions, memoryKbPeak, bytesNow}
  exceeded(h)                       -> boolean
  snapshot(h, hostStamp)            -> bytes

  // Host
  drive(id, instanceH)              -> 'alive'|'exited'|{faulted: message}
```

`release(h)` is not optional and not a courtesy. A JS bridge holds its
instances in a map keyed by handle; nothing else ever tells it that a
handle is dead, so a host that never calls `release` leaks every instance
it ever made — which is what `killing_an_instance_releases_the_js_handle`
exists to catch. It was missing from this table until 2026-08-28, so an
implementation written against the older text has that leak by
construction: check for it before assuming otherwise.

### `handleOf(id)` — settled, and why it is a seventeenth function

The swarm mints an instance id *after* `load` has already returned a
handle, so JS learns the pairing only when `drive` is first called for
that id. An instance that has been spawned but not yet driven cannot be
mapped, and a panel reading state between those two moments will not find
it. Recording the pairing on first drive works and is what the Lab does
today, but it is an inference from a call that happens to carry both
values, not a fact anyone published.

`Instance::host_token` already *is* that fact on the Rust side, so the
export is a lookup rather than new bookkeeping:

```
  handleOf(id)                      -> instanceHandle | null
```

`null` for an id the swarm does not have — including one whose instance
has been released, which is the same answer for the same reason.

Deciding it now rather than later is the whole point: the export table is
a compatibility surface the moment it ships, and a consumer written
against a table without `handleOf` will have built the fragile inference
in permanently.

`Step` is `{parked: {queues, timeoutMs, forSpace}}` or `{done: true}`.

`budget` is `{instructions, memoryKb}`, either field absent meaning
unlimited. `hostStamp` is a string or null; passing a string refuses an
unstamped snapshot, so stamping is never advisory.

**`load` takes source, not a tagged program.** `LoadSpec::program` is
`Source | Bytecode` in Rust, but bytecode is refused *before* it reaches
the bridge — the browser tier has no verifier either (GUARANTEES.md), and
keeping that refusal in one place means every host makes it identically.
So JS never sees a variant to discriminate, and there is no tagging to
agree on.

**Handles — instance and queue alike — are opaque.** JS mints them (an
index into its own table is the obvious choice) and Rust never interprets
one. `usage.bytesNow` is "held right now, bytes", which is what an
instances panel wants and what `dv_memory` answers; `memoryKbPeak` is the
high-water mark a supervisor budgets against.

## Errors: where a fault goes, and what may throw

Two questions that look like one.

**Which fault.** The Rust side has four `EngineError` variants and the
boundary routes by *which import threw*, not by anything JS says:

| import that threw | becomes | meaning |
|---|---|---|
| `run`, `resume`, `resumeTimeout` | `Program` | the guest's fault; the instance's fate, not the engine's |
| `load` | `Program` | the program was rejected |
| `restore` | `Engine` | (`SnapshotMismatch` once the shim distinguishes a refused header) |
| `queueInfo`, `push`, `pop`, `snapshot` | `Engine` | the engine failing at its own job |

So a guest raising an error surfaces as a throw from `resume`, and the
swarm reports it as a faulted instance — which is the channel the panel
already distinguishes.

**What may throw.** Only the imports whose Rust signature is fallible; the
rest have nowhere to put an exception:

- **May throw** (the shim catches it and makes it an `EngineError`):
  `load`, `restore`, `queueInfo`, `push`, `pop`, `run`, `resume`,
  `resumeTimeout`, `snapshot`.
- **Must not throw:** `abiVersion`, `release`, `queue`, `currentWait`,
  `usage`, `exceeded`, and **`drive`**.

`drive` is the important one, and it is why it returns a three-way value
rather than signalling by exception: a fault is `{faulted: message}`, a
*value*. `swarm.js` already reasons exactly this way — "unwinding the wasm
stack out of `dvs_step` is worse than an instance that does not advance" —
and the same is true here with `step` in place of `dvs_step`. An import
that must not throw and does will abort the module rather than fail
gracefully, so the shim should catch and return a safe default at its own
boundary.

## Copies

`push`, `pop` and `snapshot` each copy their bytes across wasm-bindgen, so
a message under `step()` is copied JS→wasm→JS. That is worth knowing before
someone benchmarks it and is surprised, and it is an accepted cost: the
browser is not the performance tier (see the note on boundary crossings in
`crates/drt-web/src/engine.rs`). JS can avoid a further copy on its own
side with `dv_queue_peek`/`dv_queue_release` instead of a popping read.

## `drive` is synchronous, and that is why the Lab's deferred pattern fits

`SwarmHost::drive` is a synchronous trait method. The swarm drives
instances synchronously; that is the design, not an oversight, and it is
why the native pump uses `pollster`.

This is the piece that matters most for the migration, because it means
**the Lab's existing machinery is already the right answer**, not something
to preserve grudgingly. `js_host_drive` is called from wasm during a step
and returns without awaiting; a hostcall that cannot answer immediately
returns `{status: 'pending'}`, the promise settles later, and `_settled`
delivers the reply on a subsequent step. That is exactly what a synchronous
`drive` requires in a browser, and it is already built and working.

So the Lab takes `Swarm` for bookkeeping and keeps its own pump. It does
**not** take `PumpHost`/`Dispatcher`: those await connectors on the spot,
which cannot work on a browser main thread.

For the CDN audience the calculus differs — an app developer cannot be told
to implement a pump — so `drt-web` will eventually ship a deferred pump of
its own, built to this same shape. That is additive and does not change
anything above.

## Open, deliberately

Whether `drt-web` also exports a higher-level app API (declare a program,
grant capabilities, dial a peer over `webrtc://`) beside the swarm surface,
or whether that is a second crate over this one. The swarm surface is the
common core either way, which is why it is specified first.
