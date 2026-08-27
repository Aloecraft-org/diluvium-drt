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

## Four corrections, and the guard each one bought

Every one was the same mistake in different clothes: **something differing
between the two measurements that was not the runtime, and was invisible.**

| # | the artefact | the guard now in place |
|---|---|---|
| 1 | A single-sample claim, against a 74% run-to-run band | `--repeat N`, field-wise medians |
| 2 | The harness re-encoded its payload per message; the C encodes once | `p*_allocs_per_roundtrip` printed beside every timing (the C's is zero) |
| 3 | RSS read from a whole-suite run, conflating fixed cost with per-agent | `rss_baseline_bytes` captured at process start |
| 4 | Timings compared across machine states hours apart | `bench/ab.sh` — interleaved capture, both files written together |

A fifth was caught before it was published: `us_per_lookup` timed the *last*
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
| one handle lookup | 0.067 µs | **0.0093 µs** | 7.2× |
| full roster walk (256) | 17.1 µs | **2.4 µs** | 7.2× |
| idle step, 64× table headroom | 298 µs | **91 µs** | 3.3× |
| idle step, 8× headroom | 125 µs | **92 µs** | 1.4× |

The idle-step win is the same property twice: the C's cost scales with the
table *bound*, DRT's with what is actually claimed.

**Message path: DRT is 13–24% slower, and this is the real remaining gap.**

| payload | C | DRT | |
|---|---|---|---|
| 16 B | 2.42 µs | 2.99 µs | 1.24 |
| 256 B | 3.10 µs | 3.69 µs | 1.19 |
| 4 KB | 7.35 µs | 8.31 µs | 1.13 |

The handle index did not move this — with 64 agents the scan it replaced was
short. The six allocations per round trip were split by phase rather than
guessed at:

| phase | before | after |
|---|---|---|
| `Swarm::push` | 0.05 | 0.05 |
| the drive loop | 4.05 | **2.03** |
| the harness's drain | 2.00 | 2.00 |

Two of the drive loop's four were DRT's own: `WaitSet` held a `Vec` where
`dv_waitset` hands over a fixed `dv_queue_id ids[DV_WAIT_MAX]`, and `drive`
lifts a wait twice per step (once for `current_wait`, once for the `Parked`
a step returns). `WaitSet` now carries the same fixed array, and the count
drops from 6.09 to 4.08. The other two are `diluvium::Wait`'s own
`Vec<QueueId>`, built inside the safe wrapper; they close upstream, by
giving `Wait` the same treatment.

## The allocations were not the cause. That theory is dead.

Removing a third of them — a change that is exactly and only that, measured
old-binary-against-new interleaved, seven passes each — moved the message
path by **nothing**:

| payload | allocs 6.09 → 4.08 | time |
|---|---|---|
| 16 B | −33% | 1.04 |
| 256 B | −33% | 0.96 |
| 4 KB | −33% | 0.99 |

Scattered either side of 1.0, and the 16-byte case came out *slower*. In the
same pair of runs the idle-step scenarios moved between 0.88 and 1.11 on
identical code paths, so ±11% is this machine's noise floor — larger than
the whole effect being looked for.

This branch had already been careful to say the allocations were *traced*
and not *shown to cause* the gap. They do not. Two things follow. The
`WaitSet` change stays, because two allocations per message to move at most
32 `u32`s is bad code whatever the clock says, and the upstream ask stays
for the same reason — but neither is a performance fix, and neither should
be sold as one. And the 19–29% is still unexplained.

One trap for whoever picks it up. The gap looks like it has a shape across
payload sizes, and it does not: the capture before this one read 1.20 / 1.19
/ 1.29 and this one reads 1.24 / 1.19 / 1.13, so the 4 KB column alone moved
by more than the trend anyone would read into either. Do not build a theory
on that ordering. What survives both captures is only the flat fact that all
three are slower by roughly a fifth.

So the next measurement to take is a **count**, not a time — FFI crossings
per round trip on each side. A count is the comparable figure here, and a
difference in one is a real difference rather than the container's mood;
that is the whole doctrine below, applied to the one question still open.

**Per-instance work is at or near parity**, and moves between passes:
hibernate 0.99, wake 1.03, small spawn 1.07, subtree kill 0.85–1.07, tight
idle step 0.89. Large-program spawn is consistently ahead (0.75) because the
C copies each request through two 32 KB stack buffers whatever its real size.

**Memory: 5% higher per agent, from a fixed cost that amortises.** Fitted
across four agent counts with each harness run alone: C carries 0.72 MiB
fixed and 17.7 KiB per agent beyond the guest heap; DRT carries 3.75 MiB
fixed and **16.6 KiB** per agent. The guest heap is identical to the byte, so
this is each runtime's own overhead. Total RSS crosses over as a deployment
grows — 21% higher at 128 agents, 5% at 512, 1.9% at 1,024, still closing.

**Not ported:** `churn` (needs the host-side LRU residency policy — which is
also what `drt start` will need, so it is worth doing before building on the
residency story) and `jwt` (measures the interpreter doing HMAC-SHA256, the
same C core on both sides, so it cannot distinguish the swarm layers).

## Footprint, for the distribution question

`cargo build --profile release-small -p drt`, stripped:

| profile | size | carries |
|---|---|---|
| `slim` (default) | 1.22 MiB | engine, swarm, `time`, `fs`, `crypto` |
| `full` | 3.61 MiB | the above plus `sql` (SQLite bundled) and `ssh` (russh) |
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
