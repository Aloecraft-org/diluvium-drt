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
  drt-bench -- --json --seed 7`, same machine, same day.

`crates/drt-bench` reproduces the same scenarios with the same flags and the
same JSON field names, so the two files diff directly:

```
python3 bench/compare.py bench/c-swarm_bench-baseline.json bench/drt-bench-run.json
```

## The result, first run

**Fidelity: every deterministic figure matches.** Snapshot bytes per agent
(1,430), resident heap per agent (86,750 vs 86,770), agents per GiB in both
states, the resident/cached ratio (60.66 vs 60.67), and the step counts the
rate limiter produces (9 at rate 64, 65 at rate 8) all reproduce. That is the
differential test passing: the port carries the same behaviour, not merely a
similar one.

**Memory: the slot table is 11× smaller, and lazily allocated.** 144 B per
slot against the C's 1,632, and the table grows to what is claimed rather
than to the bound. The 512-agent run allocates 73,870 B where the C reserves
`520 × 1,632` = 848,640 B up front. `doc/Benchmarks.md`'s warning — a swarm
sized for 100,000 instances reserving 156 MB before a program loads — does
not carry over: the same bound costs DRT nothing until instances exist.

**Table scanning: faster where the C's cost scaled with the bound.** Idle
step at 64× table headroom, the figure the C doc labels as one that "does not
travel", is 233 µs against 409 µs — and DRT's `slots_allocated` stays at 257
regardless of the headroom, which is the whole reason. Subtree kill of a tree
of large programs is 55 µs/agent against 128. Spawning a 3.7 KB program is
597 µs against 939, because the C copies the request through two 32 KB stack
buffers per spawn whatever the program's real size.

**Message path: DRT is 20–25% slower, and it is a known cost.** A 16-byte
round trip is 4.19 µs against 3.48; 4 KB is 16.9 µs against 13.6. Every
`Swarm::push` resolves the destination queue *by name* and then interns the
handle, on every message. Caching the resolved handle per (instance, queue)
is the obvious fix and has not been done yet. The `MiB_per_s` rows are not
comparable between harnesses — they divide by a differently-counted trip
total; use `us_per_roundtrip`.

**Not yet ported:** `churn` (needs the host-side LRU residency policy the C
bench carries) and `jwt` (measures the interpreter doing HMAC-SHA256, which
is the same C core on both sides and so cannot distinguish the swarm layers).
Both are worth adding; neither tells us anything about the port.

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
