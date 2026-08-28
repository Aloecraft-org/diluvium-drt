# The browser tier: the contract between `drt-web` and its JS host

SPEC.md §4 designates the Lab's JS host as DRT's browser implementation.
This file is the interface between them, and DRT defines it for the same
reason DRT defines the relay's wire format: whoever owns the trait writes
the contract, and the other side consumes it.

**Status:** specification. `crates/drt-web` is not written yet; this exists
first so the JS side can be built against it in parallel rather than after.

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

One object, supplied at construction. Fifteen functions: the `Engine`
trait's three, `Instance`'s eleven, and `drive`.

```
  // Engine
  abiVersion()                      -> number
  load(spec)                        -> instanceHandle
  restore(spec)                     -> instanceHandle

  // Instance, per handle
  queue(h, name)                    -> queueHandle | null
  queueInfo(h, q)                   -> {len, capacity, enabled, exported}
  push(h, q, bytes)                 -> 'accepted'|'droppedOldest'|'full'|'disabled'
  pop(h, q)                         -> bytes | null
  run(h)                            -> Step
  resume(h, firedQueue)             -> Step
  resumeTimeout(h)                  -> Step
  currentWait(h)                    -> {queues, timeoutMs, forSpace} | null
  usage(h)                          -> {instructions, memoryKbPeak}
  exceeded(h)                       -> boolean
  snapshot(h, hostStamp)            -> bytes

  // Host
  drive(id, capsHandle, instanceH)  -> 'alive'|'exited'|{faulted: message}
```

`Step` is `{parked: {queues, timeoutMs, forSpace}}` or `{done: true}`.
Errors are thrown; a throw becomes an `EngineError` on the Rust side.

An instance handle is whatever JS wants it to be — an integer index into
its own table is the obvious choice. Rust treats it as opaque.

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
