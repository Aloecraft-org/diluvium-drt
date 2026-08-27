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
| one handle lookup | 0.044 µs | **0.0063 µs** | 7.0× |
| full roster walk (256) | 11.2 µs | **1.6 µs** | 6.9× |
| idle step, 64× table headroom | 190 µs | **73 µs** | 2.6× |
| idle step, 8× headroom | 82.9 µs | **70.4 µs** | 1.2× |

The idle-step win is the same property twice: the C's cost scales with the
table *bound*, DRT's with what is actually claimed.

**Message path: DRT is 19–29% slower, and this is the real remaining gap.**

| payload | C | DRT | |
|---|---|---|---|
| 16 B | 1.60 µs | 1.92 µs | 1.20 |
| 256 B | 2.27 µs | 2.71 µs | 1.19 |
| 4 KB | 6.85 µs | 8.84 µs | 1.29 |

The handle index did not move this — with 64 agents the scan it replaced was
short. The six allocations per round trip are now split by phase rather than
guessed at, and the answer was not the one assumed:

| phase | allocations per round trip |
|---|---|
| `Swarm::push` | **0.05** |
| the drive loop | **4.05** |
| the harness's drain | 2.00 |

The push path is already clean — the handle cache did that. The cost is the
**wait set**, and it is upstream: `dv_waitset` is a fixed-size C struct
(`dv_queue_id ids[DV_WAIT_MAX]`), but the safe `diluvium` crate copies it
into a `Vec<QueueId>` for every `current_wait()` and every `Step::Parked`.
`StepHost::drive` triggers both per step, so four allocations per message
exist purely to move a small fixed array onto the heap and back.

The fix is upstream and small: make `Wait` hold the fixed array the ABI
already hands over. DRT's own `WaitSet` should mirror it. Until then, a host
can halve it by caching the wait set it was last handed instead of asking
again with `current_wait()`.

The remaining two are the harness's drain — `queue()` builds a `CString` for
the FFI name (bounded by the 64-byte queue-name limit, so a stack buffer
would do) and `pop()` returns an owned `Vec` where the C peeks a borrowed
span.

**Per-instance work is at or near parity**, and moves between passes:
hibernate 1.08, wake 1.15, small spawn 1.23, subtree kill 0.86–1.16, tight
idle step 1.05. Large-program spawn is consistently ahead (0.84) because the
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
| `slim` (default) | 1.13 MiB | engine, swarm, `time`, `fs` |
| `full` | 3.53 MiB | the above plus `sql` (SQLite bundled) and `ssh` (russh) |
| `full`, system SQLite | 2.54 MiB | the same, linking `libsqlite3` instead |

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
