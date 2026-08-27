# Benchmarks: the baseline, and what is comparable

DRT's benchmark story is a comparison, not an absolute: the question is how
the Rust swarm stacks up against the C `dvs.c` it replaces. That comparison
is only meaningful **same-machine**, so this directory holds a captured run
of diluvium's own `swarm_bench` from the machine DRT's numbers will be taken
on.

- [`c-swarm_bench-baseline.json`](c-swarm_bench-baseline.json) — `make
  swarm_bench ARGS="--json --seed 7"` at `--scale 1`, from
  `aloecraft-org/diluvium` at `a9a6258`, gcc 13.3 `-O2`.
  Machine: 4-vCPU Intel Xeon @ 2.80 GHz, Linux container.

- [`drt-bench-run.json`](drt-bench-run.json) — `cargo run --release -p
  drt-bench -- --json --seed 7 --repeat 5`, same machine, same day.

Both files are **medians of five runs** (`drt-bench --repeat N` medians
field-wise; deterministic fields are unaffected, since their median is their
value).

## Both sides are captured interleaved. That is not optional.

`bench/ab.sh N` alternates C, DRT, C, DRT… and medians each side. Capturing
one and then the other — even an hour apart — compares two machine states,
not two implementations, and on a shared container that difference dwarfs
everything being measured.

This branch learned that the hard way. `tight_table_us_per_step` read 184 µs
from a C baseline captured in the morning and 67–91 µs from the *same binary*
that afternoon. Every ratio quoted against that baseline — including a
confident "the message path is 35–40% faster" — was measuring the
container's mood.

## Five corrections, and the guard each one bought

Every one was the same mistake in different clothes: **something differing
between the two measurements that was not the runtime, and was invisible.**

| # | the artefact | the guard now in place |
|---|---|---|
| 1 | A single-sample claim, against a 74% run-to-run band | `--repeat N`, field-wise medians |
| 2 | The harness re-encoded its payload per message; the C encodes once | `p*_allocs_per_roundtrip` printed beside every timing (the C's is zero) |
| 3 | RSS read from a whole-suite run, conflating fixed cost with per-agent | `rss_baseline_bytes` captured at process start |
| 4 | Timings compared across machine states hours apart | `bench/ab.sh` — interleaved capture, both files written together |
| 5 | The harness counted *pushes* where the C counts *drained replies* | `drain_done` returns the count, and a settle loop lands what is in flight |

A sixth was caught before it was published: `us_per_lookup` timed the *last*
handle repeatedly (a full-table scan, the worst position) against the C's
figure, which is *derived* from a whole-roster walk and so is the average
position. That is a factor of two before any implementation difference. It is
now derived identically.

It was never codegen. LLVM is deterministic for a given build; run-to-run
variance in an identical binary comes from allocator state, page faults and
address-space layout.

**Checked automatically:** `bench/check-fidelity.py` asserts the
deterministic fields against the committed baseline, and CI runs it on every
push. Timings deliberately are not asserted — on a shared runner that is a
check that fails when the runner is busy. Bytes, counts, the step counts the
rate limiter produces, and allocations per round trip all reproduce.

## The result

Interleaved, medians of five passes each.

**Fidelity: every deterministic figure matches.** Snapshot bytes per agent
(1,430), resident heap per agent, the resident/cached ratio, and the step
counts the rate limiter produces (9 at rate 64, 65 at rate 8). The port
carries the same behaviour, not merely a similar one.

**Anything that touches the instance table is far ahead.** `dvs.c` resolves a
handle by scanning the slot table, and its own bench notes that "every call
that takes a handle pays it… a host walking its own roster is quadratic in
the swarm size". DRT keeps a handle→index map, so:

| | C | DRT | |
|---|---|---|---|
| one handle lookup | 0.056 µs | **0.0080 µs** | 7.0× |
| full roster walk (256) | 14.3 µs | **2.1 µs** | 6.9× |
| idle step, 64× table headroom | 334 µs | **85 µs** | 3.9× |
| idle step, 8× headroom | 116 µs | **90 µs** | 1.3× |

The idle-step win is the same property twice: the C's cost scales with the
table *bound*, DRT's with what is actually claimed.

**Message path: DRT costs a flat ~0.45 µs more per round trip. It is a
constant, not a percentage.**

| payload | C | DRT | ratio | absolute |
|---|---|---|---|---|
| 16 B | 2.14 µs | 2.54 µs | 1.19 | +0.40 µs |
| 256 B | 3.05 µs | 3.53 µs | 1.16 | +0.48 µs |
| 4 KB | 9.48 µs | 10.00 µs | 1.05 | +0.52 µs |

Every earlier version of this section quoted the ratio — "35–40% faster",
then "19–29% slower", then "13–24% slower" — and the ratio was the wrong
statistic all along. The overhead barely moves across a 256× range of
payload sizes; what moves is the denominator. Reading a trend into 1.19 /
1.16 / 1.05 is reading the payload size back out of a division.

So the thing to explain is **0.45 µs of fixed cost per round trip**, and
three separate attempts to explain it have now failed:

| what was removed | measured effect on the clock |
|---|---|
| 3 of the 6 heap allocations on the hot path | none — 1.04 / 0.96 / 0.99 |
| one FFI crossing per round trip (`dv_waitset_get`) | none — 1.04 / 1.00 / 1.04 |
| a metric bug: counting pushes, not drained replies | none — they were equal |

Each was measured old-binary-against-new, interleaved, seven passes each, so
the only difference was the change itself. Each was a real defect and each is
fixed. None of them was the cost — and in the same runs the idle-step
scenarios moved between 0.88 and 1.11 on *identical* code, so this machine's
noise floor is wider than anything being looked for.

A fourth check closed the last open flank: whether the *guest* executes
identical work. The instruction hook cannot answer it — the C's own
`--count` reads a flat ~1,000 instructions total for this scenario against
4,096 round trips, startup-scale, so neither side's counter sees the echo
loop — but a stronger instrument already had: the snapshot fidelity fields
are byte-identical (1,430 B/agent), and no divergent execution produces an
identical heap. The guest side is settled; the constant lives in the host
wrapper.

The remaining shape of the evidence — a constant, immune to removing
individual items — says the cost is **distributed**: roughly 65 ns across
each of the ~7 wrapper crossings a round trip makes, which is about what
`dyn` dispatch, a safe wrapper and bounds-checked handle resolution cost when
you add them up. If that reading is right there is no single fix, and the
honest thing is to stop hunting one.

Two genuine seam gaps surfaced on the way. Both are worth closing on their
own merits, and neither should be sold as performance work:

- **The seam has no non-consuming peek.** The C's drain is `dv_queue_peek` +
  `dv_queue_release` — a borrowed span, no copy. DRT's `Instance::pop` hands
  back an owned `Vec`, so every reply costs an allocation and a payload copy
  the C never makes. That is `p*_allocs_drain`, and at 4 KB it is a 4 KB
  memcpy per message.
- **Looking a queue up by name allocates.** `dv_queue_lookup` takes a
  `const char *`; the safe wrapper builds a `CString` for it. Queue names are
  bounded at 64 bytes, so a stack buffer would do. The runtime path does not
  pay this — `Swarm::push` caches handles per residency — but the harness's
  drain does, once per agent per round.

Allocations per round trip, for the record: **6.09** when this started,
**3.06** now — `push` 0.05, the drive loop 1.02, the harness's drain 2.00.
Two of the three removed were `WaitSet` holding a `Vec` where `dv_waitset`
hands over a fixed `dv_queue_id ids[DV_WAIT_MAX]`; the third was
`StepHost` asking `current_wait()` for a park `resume` had just returned to
it. The drive loop's last one is `diluvium::Wait`'s own `Vec<QueueId>`,
built inside the safe wrapper, and closes upstream by the same change.

**Per-instance work is at or near parity**, and moves between passes:
hibernate 1.03, wake 1.00, small spawn 1.05, subtree kill 0.89–0.98, tight
idle step 0.89. Large-program spawn is consistently ahead (0.75) because the
C copies each request through two 32 KB stack buffers whatever its real size.

**Memory: 5% higher per agent, from a fixed cost that amortises.** Fitted
across four agent counts with each harness run alone: C carries 0.72 MiB
fixed and 17.7 KiB per agent beyond the guest heap; DRT carries 3.75 MiB
fixed and **16.6 KiB** per agent. The guest heap is identical to the byte, so
this is each runtime's own overhead. Total RSS crosses over as a deployment
grows — 21% higher at 128 agents, 5% at 512, 1.9% at 1,024, still closing.

**Churn — oversubscription under the same die and the same policy — now
reproduces the C's cache behaviour exactly**: the same seed drives the C's
own xorshift64* and the same LRU (ties evict the highest index, as the C's
`<=` scan does), so hit rates (1.0 / 0.5127 / 0.1143), wakes (0 / 499 /
907), hibernates, step counts and the 16-of-64 wake-buffer bound all match
to the digit, and the fidelity gate asserts all of them. What the clock adds
on top: nothing worth naming — interleaved, the three labels land at
0.98–1.05 of the C on ops/s and per-wake alike.

**Not ported:** `jwt` (measures the interpreter doing HMAC-SHA256, the same
C core on both sides, so it cannot distinguish the swarm layers).

## Footprint, for the distribution question

`cargo build --profile release-small -p drt`, stripped:

| profile | size | carries |
|---|---|---|
| `slim` (default) | 1.36 MiB | engine, swarm, `time`, `fs`, `crypto`, `listen` |
| `full` | 3.99 MiB | the above plus `sql` (SQLite bundled), `ssh` (russh) and `tunnel` (SSH over WSS) |
| `full`, system SQLite | 2.62 MiB | the same, linking `libsqlite3` instead |

**`crypto` costs 90 KiB, measured** — SHA-256, SHA-1, HMAC, base64 and the
CSPRNG, which is the whole `host:crypto/*` family. It is in `slim` because
that is the profile meant to travel: a program that can hash, mint a JWT and
ask for CSPRNG bytes without a network round trip is exactly what the
browser-and-notebook distribution story wants, and 90 KiB is not a price
worth arguing over. `serde_json` is already in the binary for the root
config, so the JWT payload's JSON costs nothing extra.

**Bundling SQLite costs 0.99 MiB, measured.** The alternative is what
`diluvium-host` does: `dhost_sql.c` includes `<sqlite3.h>` and links the
system library, which is why cap1's container runs `apk add sqlite`. It
bundles here because cap1's claim is that one binary carries the runtime,
and a connector that needs a library installed first is the two-binary cliff
wearing another hat. A megabyte is the price of that claim; flipping it is
one line in `connectors/sql/Cargo.toml` if a deployment would rather pay the
dependency instead.

`slim` stays at 1.13 MiB either way — `sql` is in `full`.

## What is actually comparable

Both stacks run the *same C core* underneath — DRT's engine is diluvium via
the `dv.h` instance ABI. So:

**Deterministic, must match to the byte.** Guest heap per agent, snapshot
bytes per agent, wake-buffer accept/refuse counts, VM instruction counts
under `--count`. A difference here is a bug in the port, not a performance
story. (Confirmed in the captured run: slot table 1,632.25 B/slot, cached
1,430 B/agent, 750,868 agents/GiB cached, 16 accepted / 48 refused of 64 —
all reproducing diluvium's published table exactly.)

**Same work, different wrapper — small deltas expected.** Hibernate and
wake (identical `dv_snapshot`/`dv_restore`, plus DRT's `Vec` copy), message
round trip (identical queue push/pop, plus DRT's handle interning), spawn
(identical `dv_new`/`dv_load`, but DRT parses the request through `rmpv`
into an owned `Value` tree where the C reads it with a zero-copy cursor —
DRT is expected to be *slower* here, and by how much is worth knowing).

**Different by construction — DRT should win, and the C figures do not
apply.** The C allocates the slot table up front at `max_instances × 1,632
B`, which is why `doc/Benchmarks.md` warns that a swarm sized for 100,000
instances reserves 156 MB before a program loads, and why its idle-step
figure at 64× table headroom is labelled as one that "does not travel".
DRT's `Vec<Slot>` grows to what is actually claimed and `step` walks only
that, so both costs scale with live instances rather than with the bound.
Expect these two rows to diverge; that divergence is the point.

## Times are advisory, and here is the evidence

The captured run is on the same CPU model and core count as the reference
run in `doc/Benchmarks.md`, and still lands 1.3–1.7× slower on the
allocation-heavy paths (hibernate 416 µs vs ~284 µs; wake 759 µs vs ~535 µs;
small spawn 604 µs vs ~394 µs) while matching or beating it on the scanning
paths (idle step at 256 agents, 173 µs vs ~188 µs). Container overhead,
build drift, or both.

One byte figure also moved: resident heap per agent reads 86,765 B here
against the ~73 KB in the published table. Byte figures are supposed to be
deterministic, so that is diluvium version drift rather than noise — the
published table is stale against current `main`, which is the whole reason
this baseline is captured rather than quoted.

**So: compare DRT against this file, never against the numbers in
`doc/Benchmarks.md`.** Re-capture it whenever the machine or the diluvium
revision changes.
