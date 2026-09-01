# What is next, sized

Written 2026-09-01, at v0.4.0. Everything here was sized by reading the
code, not by estimating from a description, and each entry names the file
and line the estimate rests on so nobody has to re-derive it.

**Three of these corrected a claim made in the request that prompted
them.** Those corrections are kept in place rather than quietly fixed,
because a wrong shared belief about where the cost is will be re-derived by
the next person otherwise.

The first four are independent, unblocked, and about two days together.

---

## Unblocked, small

### 1. `wall_ms` and `spawns` budgets — ~1 day

`Budget` is `{instructions, memory_kb}` (`drt-config/src/lib.rs:27`). Both
are properties of the guest VM. Neither bounds the two things a swarm
actually spends — wall-clock time and cumulative spawns in a lineage.
`instructions` cannot express wall time at all: a hostcall that parks for
85 seconds consumes none.

`fits_within` and `resolved_against` (`drt-config/src/lib.rs:69-85`) are
already written as a generic `fits(child, parent)` over `Option<u64>`
applied per field, so the attenuation semantics — `None` inherits, a child
may state smaller and never larger — come free for new dimensions.

**Correction worth keeping: this needs no `diluvium` change.** The request
that raised it said "plus `dv_set_budget`'s surface in `diluvium`". Not for
these two. `instructions` and `memory_kb` have to go through
`dv_set_budget` because they are VM properties. `wall_ms` and `spawns` are
host-enforced by construction — the host is the only thing that *can* see
them — so this is pure DRT: no ABI bump, no pin move, no cross-repo
coordination, which is usually the expensive part.

Enforcement has a home already: `DeployHost` carries a tick counter and
`Instant` deadlines, and every slot carries `parent`, so a cumulative
lineage counter is an increment along an existing chain.

Deliberately deferred out of v0.4.0: it changes config parsing, and this
repository's own traps list says to re-run the examples when that happens.
It also adds fields the C host does not have, which is divergence in the
one direction the priority order forbids — so the *shape* wants agreeing
with `dhost.c` before it ships, not after.

### 2. The scheduler — ~½ day, or weeks

Today the only way to run something later is an instance awake on a
timeout, which scales with the number of scheduled things rather than with
work.

**Correction: most of the timer already exists.** `next_deadline`
(`drt/src/start.rs:206-209`) already computes the earliest pending park
deadline across the roster, and the drive loop already sleeps toward it
(`start.rs:326-328`). Two concrete gaps:

- `.min(IDLE_TICK)` caps the sleep at 1 ms, so an idle deployment still
  wakes a thousand times a second however far off the next deadline is.
  Letting the sleep run to the deadline when nothing else needs a tick is
  nearly a one-liner.
- `detached` (`start.rs:154-156`) drops the park — deadline included — when
  an instance hibernates. So a hibernated instance has no timer, which is
  exactly the case this is supposed to serve. The deadline has to outlive
  residency.

The "must not become a supervisor type" constraint is satisfied for free:
this is a timer on a park that already exists, not a new object with a
lifecycle.

**The caveat that changes the estimate by an order of magnitude.** A
deadline in a `HashMap` dies with the process. If scheduled things must
survive a restart, the deadline belongs in the snapshot store, and that is
weeks, not hours. Decide which is being asked for before starting.

Deferred out of v0.4.0 because it touches the drive loop, which is exactly
where v0.4.0's own relay-spins-a-core bug lived. Not the week of a release.

### 3. Idle step cost — ~½ day

`Swarm::step` makes three full passes over `self.slots`
(`drt-swarm/src/swarm.rs:782`, `:797`, `:802`) every step, regardless of
how many instances are alive. An active list alongside the table makes idle
cost proportional to work.

### 4. `kill_subtree` is quadratic — ~½ day

`drt-swarm/src/swarm.rs:564-589` is a fixpoint loop with a full slot scan
inside it, so tearing down a lineage is O(depth × n) — worst case O(n²) on
a deep one. A child index fixes it.

**Correction: this is the quadratic, and it is not on the path people
think.** The claim that prompted this was "a host walking its own roster is
quadratic today". That is not true: `Swarm::find` is O(1), a lookup in
`self.index` maintained on claim and release (`swarm.rs:337-342`, `:355`,
`:367`), and every per-instance accessor — `parent`, `resident`,
`wake_on_message`, `cached_size`, `budget`, `caps`, `holds` — goes through
it. `ids()` plus N accessor calls is O(n). The real quadratic is on the
teardown path, where a swarm that grows a leaf per pattern will meet it.

---

## Blocked on something else

### 5. Roster introspection in one query — ~½ day of swarm work, gated on the control endpoint

The data is all there behind O(1) accessors; aggregating it into one struct
is half a day. But `drt ps` is a stub that says so
(`drt/src/main.rs`): it reaches a running deployment over the control
endpoint, which SPEC §13a lands with sshd.

So this is not gated on swarm work at all. If the notebook is what is
blocked, **the control endpoint is the dependency to schedule** and the
roster query is a small thing hanging off it.

---

## The browser tier

### 6. A verified browser release — ~1 week minimal, 2-3 weeks useful

`drt-web` became a second-class citizen, and there is a visible feedback
loop in this repository's own reasoning about why. `ci.yml:80-87` and
`doc/Release.md` both argue — correctly — that wasm32 stays out of the
release matrix because every other leg proves itself by *running* the
artifact and a wasm one cannot be run on the runner. Sound, and
self-reinforcing: it cannot ship because it cannot be verified, and nobody
builds the verification because it does not ship.

**What is already there, which is more than "unwired":**

- `HostBridge` is the JS contract as a Rust trait, and a mock bridge drives
  a **real `Swarm`** under ordinary `cargo test`. The seam was kept honest,
  which is why this is wiring rather than a rewrite.
- `wasm-bindgen` and `js-sys` are already declared under
  `cfg(target_arch = "wasm32")`, with the `default-features = false` trap
  on `drt-swarm` already found and documented in that Cargo.toml.
- `doc/Browser.md` has the whole contract designed: a 16-row export table,
  the panic-safety rule, and the throws / must-not-throw table.
- CI compiles `drt-web` for wasm32 on every push, green.
- The interpreter half exists and works: `diluvium/bindings/js` is a real
  package — `instantiate()`, `Instance`, `Wait`, `Step`, msgpack, a WASI
  shim — and its `instance.integration.mjs` is wired into that repo's CI.

**What is missing:**

| piece | size |
|---|---|
| `#[wasm_bindgen]` export layer | 1-2 days — 16 methods, each a panic-wrapped shim over an existing O(1) `Swarm` accessor |
| Import glue (a JS object bound to `HostBridge`) | 1-2 days |
| Playwright harness and a first real test | ~1 day |
| `BUILDINFO` `profile.web.exports` and the matrix leg | ~½ day |
| Connector/pump layer (the third piece of #31) | ~1 week, and the only one with real unknowns |

**Raise the smoke bar before meeting it.** `doc/Release.md`'s stated
trigger for the wasm32 leg is a **node** smoke step asserting
`abiVersion() === 1`. The one real browser-vs-native divergence this
project has actually observed is in `doc/HostBaseline.md`: Lab's REPL
cannot answer `host.time()` at all, because it evaluates on a thread that
cannot park, so the queue round-trip has nowhere to yield. A node smoke
test will never see that — node's event loop is not the browser's — and it
is precisely the "sorry, that's different in Lab" failure the browser-first
plan existed to prevent. Playwright is not gold-plating the gate here; it
is the only proposed test that catches the failure mode already observed
once.

**Risks to settle before starting, not after:**

1. `diluvium/bindings/js` is `diluvium` v0.1.0 and appears unpublished. A
   browser release has a hard dependency on that package reaching
   consumers, and `doc/Release.md`'s mirror ask covers binaries only.
2. DRT has no JS tooling today — no `package.json`, no node in CI. This
   adds `wasm-bindgen-cli` to the release job and a node step to `ci.yml`.
3. Panic-safety is per-export. It is easy to wrap fifteen of sixteen, and a
   panic crossing the boundary is UB rather than an exception. Worth a test
   that deliberately panics one.
4. `swarm.integration.mjs` in diluvium **has never executed anywhere** —
   its own header says so, because building its module needs the wasi-sdk
   container. That is the *C* swarm's JS route, which `drt-web` replaces,
   so it does not block; but the JS-side swarm path has no prior green run
   to lean on.

---

## Not in this list

**FM-2's upstream fix** has its own brief: `doc/FM-2-Upstream.md`. DRT's
mitigation shipped in v0.4.0 and the real repair is diluvium's.

**The `v*` tag ruleset** that 403'd v0.3.1 is unchanged and is not an
engineering task: an admin clears the rule, or a human account publishes
from the Releases page. See `doc/Release.md`.

**Token budgets** are in neither list because they are not a sizing
question. With a generic `rest` connector the response body lands in the
guest, so the count is self-reported by the thing being budgeted, and no
amount of implementation makes that enforceable. The two honest options —
host-side call/byte metering at the queue layer (SPEC §7 designs it and
nothing builds it, a few days) and a purpose-shaped `llm` connector that
owns the token meter — compose, and the second buys enforceability at the
price of the host knowing one API's response shape.

Worth noting v0.4.0 moved this slightly: the `rest` connector's scope can
carry operator-injected headers, so the host already holds the credential
and already sees the response bytes. A token meter could live behind that
same scope without a new capability family — it would still have to parse
one API's usage block, but it would not need a second grant.
