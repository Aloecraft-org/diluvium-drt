# The determinism corpus

Programs whose every output line is a claim that DRT gives the same
answer on every target it ships. The layout is the examples' (`NN-*/`
with `meta.json`, a program and `expected.txt`), so the three existing
runners run it unchanged with `EXAMPLES_DIR` pointed here:

```sh
EXAMPLES_DIR=tests/determinism DRT=target/release/drt examples/run-all.sh
EXAMPLES_DIR=tests/determinism DRT=script/drt-wasip2.sh examples/run-all.sh
cd crates/drt-web/browser-test && EXAMPLES_DIR=../../../tests/determinism node run.mjs
```

CI runs all three, and natively both `full` and `slim`.

| corpus | holds on | what it pins |
|---|---|---|
| `01-numeric` | builds with `numeric` (`full`, `web`) | diluvium's own `test/numeric/corpus.lua` and `expected.txt` at the pinned core, vendored unchanged: array kernels and the vendored libm, as IEEE bits |
| `02-exact` | every profile | integers, float printing, the default random seed, iteration over scalar keys, sort with ties, floats through `json` and `msgpack` |
| `03-reproducible` | builds with `numeric` | `math` and `^` through the vendored libm, and iteration over table and function keys |

Floats are compared as bits except where printing is the point.

**Why two tiers.** Without `numeric` the core keeps the platform's libm
under `math` and `^` (diluvium `src/dlibm.c`, `src/luaconf.h`), so those
bits are the platform's: measured, native `slim` on glibc answers
`math.atan(π)` one bit away from every `numeric` build. And iteration
over reference keys depends on how many objects the build's setup made,
so it agrees only between builds with the same core features. Both are
the core's design today, stated in `doc/Snapshot-Identity-Upstream.md`.

**What is not here.** Hibernation is not a target but it is a boundary,
and across it iteration over reference keys changes today: the core's
snapshot does not carry key identities. That is pinned by a Rust test,
not by this corpus (`crates/drt-swarm/tests/swarm.rs`,
`pairs_order_across_a_hibernation_is_pinned_as_the_core_has_it`).

**When the pin moves**, re-vendor `01-numeric` from the new core's
`test/numeric/`, and re-capture `02` and `03` from a native `full` build;
a diff there is a change a program can observe, so it goes in the
changelog.
