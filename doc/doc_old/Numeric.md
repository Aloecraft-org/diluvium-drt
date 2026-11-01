# Numeric work in DRT: what the tiers promise, and what bounds it

DRT does not implement any numeric kernel. The kernels are diluvium's, and
what this document is about is the three things DRT owns: **what a tier
promises**, **what bounds the work**, and **how a column crosses the
hostcall boundary without being turned into Lua values**.

`doc/Plan-2026-09.md` is the plan this lands under and
`diluvium-numeric-spec.md` is the normative text for the tiers. Where the
two describe the same thing, the spec wins and this restates it for a
reader who is operating DRT rather than building the core.

## Status, plainly

The core carrying `numeric` is **not pinned yet**. `drt buildinfo` says
which features the embedded core has:

```
$ drt buildinfo | grep features
features: regex
```

Everything below about tiers describes what a build *will* report once a
core carrying `numeric` is pinned. Everything about the blob lane, the
config block and the `data` connector is true today. The line between the
two is marked at each point rather than left for the reader to work out.

## 1. The three tiers

Every kernel implementation carries one, and the tier is a promise about
**results**, not about speed.

| Tier | Promise |
|---|---|
| `exact` | Bit-identical by construction on every target. Integer arithmetic, NTT, decimal. |
| `reproducible` | Bit-identical to the portable kernel on every target. |
| `fast` | Nothing across targets. |

`reproducible` is the interesting one, and it is not free. It holds only
because the portable kernels are compiled with FMA contraction off, no
fast-math, standard excess precision, denormals honoured, no wasm
relaxed-simd, transcendentals from an embedded libm rather than the
platform's, reductions in a fixed order, and sorts stable with NaN ordered
last. Any one of those slipping on any one target breaks the promise on
that target only, which is exactly the failure that is hardest to see.

**A tier is reported, never enforced by fiat.** A program is not stopped
from running a fast kernel; what happens is that the fact is recorded. The
static analyzer classifies a call by the tier of its *portable*
implementation, because it cannot know which backend will be registered.
The runtime records whether a fast-tier backend actually ran. Static says
*could*; runtime says *did*.

DRT reports the runtime half. `numeric_touched_fast` is a sticky
per-instance flag: once a fast kernel runs, it stays set for the life of
the instance.

- On the roster: `Swarm::numeric_touched_fast(id)`, beside
  `Swarm::budget(id)`. This is what `drt ps` reads when the control
  endpoint exists (SPEC.md §13a); until then `ps` has no running
  deployment to ask.
- On the stop event: a `numeric_touched_fast: true` field beside
  `exceeded`/`faulted`/`exited`, so the supervisor learns it in the one
  message that says the instance is gone. Omitted when false, so a
  deployment doing no numeric work sees the event it always saw.

**Today it is `false` everywhere, and that is a fact rather than a
placeholder**: no fast-tier backend exists in this workspace or in the
pinned core, so no fast kernel can have run.

## 2. What bounds numeric work

Three different bounds, because there are three different kinds of work,
and the failure of each is a different failure.

### The instruction budget, for kernels

A kernel charges the guest's instruction budget **by element count, never
by time**: one instruction per 64 elements, checked at block boundaries.
That is what makes `exceeded` fire at the same point inside a matrix
multiply on every target, which is what replay depends on. A time-based
charge would be a different answer on every machine.

This is the core's to implement (`doc/Plan-2026-09.md` §3.4, session A).

### `numeric.max_elements` and `numeric.max_tier`, per instance

```lua
return {
  supervisor = "sup.lua",
  numeric = {
    max_elements = 10000000,     -- the most one kernel call may process
    max_tier     = "reproducible",
  },
}
```

Both attenuate at spawn under the same rule as `budget`: **a child may
state a stricter bound than its parent's and never a looser one, and a
child that states nothing inherits its parent's rather than escaping to
unlimited.** A looser `max_tier` is a child asking to be allowed results
its parent would not accept, so looseness is what the comparison uses:
`exact` is strictest, `fast` loosest.

The refusal names which of the two moved, because they fail for different
reasons and a supervisor fixing one should not have to guess.

The block is not feature-gated. A bound is meaningful on every build — "do
not run fast kernels" is an answer any binary can give — unlike `relay`,
`stun`, `turn` and `wireguard`, which name servers a build may not carry.

This is live today. The values reach the instance; the core does not yet
have anywhere to put them (`TODO(A2)` in
`crates/drt-swarm/src/engine.rs`).

### `max_bytes` on the `data` scope, for decoding

**The one an operator is most likely to get wrong**, because it does not
look like a numeric bound at all.

Decoding a parquet file is **host work**. It is not the guest's
instructions, so the instruction budget says nothing about it, and it is
not a kernel, so `max_elements` says nothing about it either. The only
bound is `max_bytes` on the connector's scope, which is why that number is
both the file bound and the memory bound: a parquet file is read whole
into memory, because the reader needs the footer and then random access to
the row groups the range touches.

Measured on this tree, release build, one `f64` column:

| Rows | File | Decode |
|---|---|---|
| 100,000 | 421 KB | 1.8 ms |
| 1,000,000 | 4.3 MB | 18 ms |

So roughly 4 ms per megabyte of parquet, and the 1 MB default costs about
that. `connectors/data/tests/decode_cost.rs` is where those come from; run
it with `--nocapture` to get your own machine's.

The decode runs on `spawn_blocking`, so the call **parks** rather than
blocking: the drive loop keeps stepping, every other instance keeps
running, and the answer lands when it lands. A synchronous decode would be
`exec`'s shape, which stops the whole deployment (`doc/Failure-Modes.md`
FM-4 is what that looks like when a guest does it deliberately).

## 3. How a column crosses

A column does not become a msgpack value. A million-row `f64` column is
eight megabytes that the encoder would walk, copy and re-tag for nothing,
and base64 would be worse; so the reply carries a **side channel** and the
payload references a column by index — `{dtype, len, blob}`, with `len` in
elements. `doc/Hostcall.md` has the encoding; the short version is that
the bytes never enter the serde encoding at all.

Two deliveries, and today there is one. A guest whose build carries
`numeric` reads each blob with `dv_reply_blob` and adopts it into an
`array` with no copy. A guest without it receives the bytes in the
descriptor's place, as a msgpack `bin`, which is a Lua string — which is
exactly what `dv_array_adopt` is specified to do in a build without the
feature. So the behaviour a guest sees does not change when the pin lands;
what changes is that a guest with arrays gets the first path.

### Nulls

Two representations, one per dtype:

- an `f64` column says null with **NaN**, and carries no mask;
- an `i64` or text column carries an optional `u8` **validity** mask,
  `valid`, with `1` where the row has a value;
- a column with no null row carries no mask at all.

The `f64` rule has a consequence worth knowing before you hit it: a NaN
that was data and a null are the same thing, in both directions. Nothing
is lost on a round trip, and a column that must tell them apart wants an
`i64` column with its own mask beside it.

### Text

Dictionary-encoded: `i64` codes and a table holding each distinct string
once. A code `c` is `uniques[c + 1]`. A million-row city column is a
million integers and a handful of strings, which is the entire reason the
representation is not a table of strings.

## 4. Printing numbers

**Never `%g`, in any example, test or expected output that has to hold on
more than one target.** Print IEEE bit patterns:

```lua
("%016x"):format(string.unpack("<I8", string.pack("<d", x)))
```

glibc, musl, wasmtime, Chromium and mingw format decimals differently from
each other, and one `expected.txt` cannot be right on all five otherwise.
`examples/23-reading-parquet` is the worked example. When a core carrying
`numeric` is pinned, `array.bits` does the same thing per element.
