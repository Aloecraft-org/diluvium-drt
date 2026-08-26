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

## Two corrections, and what stops them recurring

This file has twice reported a message-path result that was not one, so the
method matters as much as the numbers.

**First**, a single-sample claim of "20–25% slower". Five back-to-back runs
of an identical binary spanned 3.88–6.75 µs on the 16-byte round trip — a
74% band — so any single-run difference smaller than that was noise wearing
a number. Fixed by medianing.

**Second, and the real one**: the remaining gap was *this harness doing work
the C harness does not*. The C encodes its payload once before the clock
starts (`mp_filler`, then the same buffer every push); this one re-encoded a
`rmpv::Value` into a fresh `Vec` per message — three allocations and, at
4 KB, ten reallocating memcpys, 4,096 times inside the timed loop. That
single line was the entire reported deficit **and** the entire variance
excess.

Removing it moved the 16-byte round trip from 4.06 µs to 2.23 and collapsed
the spread from 74% to 16% — the same tightness as the C harness. One cause,
both symptoms.

**What stops a third recurrence:** the harness now counts its own
allocations and reports `p*_allocs_per_roundtrip` alongside every timing.
The C's number on that path is zero — it encodes once and drains through a
borrowed `dv_queue_peek`. So "this harness is doing work the C is not" is
now a printed number rather than an invisible assumption, and any future gap
can be checked against it before it is attributed to the runtime.

It was never codegen. LLVM is deterministic for a given build; run-to-run
variance in an identical binary comes from allocator state, page faults and
address-space layout — which is exactly what allocation churn drives.

## The result

**Fidelity: every deterministic figure matches.** Snapshot bytes per agent
(1,430), resident heap per agent (86,750 vs 86,760), agents per GiB in both
states, the resident/cached ratio, and the step counts the rate limiter
produces (9 at rate 64, 65 at rate 8). That is the differential test passing:
the port carries the same behaviour, not merely a similar one.

**Message path: 35–40% faster.** The 16-byte round trip is 2.23 µs against
3.48; 256 B is 3.05 against 5.08; 4 KB is 9.02 against 13.78 — 1.5–1.7×
the throughput at every payload size. The reason is one line of `dvs.c`:
`dvs_push` calls `dv_queue_lookup(inst, queue)` on **every message**, paying
a string lookup inside the guest each time, where `Swarm::push` resolves the
handle once per residency and caches it. DRT still allocates ~6 times per
round trip where the C allocates none, and wins anyway.

**Memory: the slot table is 9.7× smaller, and lazily allocated.** 168 B per
slot against the C's 1,632, and the table grows to what is claimed rather
than to the bound. `doc/Benchmarks.md`'s warning — a swarm sized for 100,000
instances reserving 156 MB before a program loads — does not carry over: the
same bound costs DRT nothing until instances exist.

**Table scanning: 2.2× faster where the C's cost scaled with the bound.**
Idle step at 64× table headroom, the figure the C doc labels as one that
"does not travel", is 214 µs against 476 — and DRT's `slots_allocated` stays
at 257 regardless of the headroom, which is the whole reason. With the table
sized tight the position reverses: 218 µs against 184, 18% behind.

**Hibernate and wake are at parity** (1.02× and 0.99×), which is the expected
answer — both call the same `dv_snapshot`/`dv_restore` underneath, so a gap
here would have meant the port was doing something extra.

**Spawning a large program is 22% faster** (578 µs against 739 for 3.7 KB),
because the C copies each request through two 32 KB stack buffers whatever
the program's real size. Small-program spawn and subtree kill are at parity.

**Process RSS is 24% higher per agent** (132 KB against 106 KB), against a
guest heap that matches to the byte — so the difference is entirely the Rust
side of the process, not the agents.

**Still worth doing**, now that the harness is not in the way: the ~6
allocations per round trip are real. Two are the engine seam's — `queue()`
builds a `CString` for the FFI name and `pop()` returns an owned `Vec` where
the C peeks a borrowed span. A borrowed-peek on the `Instance` trait would
close both. It is an optimisation on top of a win rather than a deficit to
repair.

**Not ported:** `churn` (needs the host-side LRU residency policy the C bench
carries) and `jwt` (measures the interpreter doing HMAC-SHA256, which is the
same C core on both sides and so cannot distinguish the swarm layers).

## Footprint, for the distribution question

`cargo build --profile release-small -p drt`, stripped:

| profile | size | carries |
|---|---|---|
| `slim` (default) | 1.13 MiB | engine, swarm, `time`, `fs` |
| `full` | 3.53 MiB | the above plus `sql` (SQLite bundled) and `ssh` (russh) |

`sql` bundles SQLite rather than linking a system one on purpose — cap1's
claim is that one binary carries the runtime, and a connector that needs a
library to have been installed first is the two-binary cliff wearing another
hat. It costs ~1.8 MiB, which is why `sql` is in `full` and not `slim`.

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
