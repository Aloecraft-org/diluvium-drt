# The config-loader regression corpus

Every config shape that is deployed or shipped, captured verbatim, so that
a loader change is checked against what installs actually run rather than
against what the schema says they could run.

`drt-config` is shared by everything, and the `numeric` block and the
`data` connector's `columns` list both add fields beside existing ones.
The rule this directory exists to enforce
(`doc/Plan-2026-09.md` §3.8): **any loader change that fails a corpus file
is wrong regardless of what it enables.** Every shipped install runs a
config this loader wrote; a regression here is caught by no other gate.

## What is in here

A corpus filename containing `__` is a byte-for-byte copy of the
repository file at the path you get by writing `__` as `/` —
`examples__21-wireguard__hub.host.lua` is `examples/21-wireguard/hub.host.lua`.
The name carries the provenance so the corpus needs no manifest, and
`tests/corpus.rs` fails if a copy has drifted from its source or if a
shipped shape has no copy at all.

A filename with no `__` is a shape captured from a deployment *outside*
this repository. There is one: `wg.host.lua`, what `wg.sh` writes on a
customer machine. `wg.sh` is on the customer side and in neither
repository, so that one file is reconstructed rather than copied, from
`doc/WireGuard.md` §1 and the field the plan names — `connectors = { time = {} }`,
an empty map arriving where a list is read, which is the `as_array` edge
rc6 fixed for five fields. Its own header says so. Replace it with the
real artifact the day one is available.

## The two checks

- `crates/drt-config/tests/corpus.rs` — the corpus is complete and
  verbatim. No loader involved, so it runs in any build.
- `crates/drt/tests/config_corpus.rs` — each file loads, and parses to the
  structure it parsed to before. The snapshots are in `snapshots/`,
  written on first run and compared after. That test needs the loader,
  which lives in the `drt` crate, which is why it is not in this crate
  beside the files it reads.

## Adding to it

Ship a new `.host.lua` under `examples/` and `corpus.rs` fails until you
copy it here; that is the point. Regenerate a snapshot only when you meant
to change what a config parses to:

    DRT_CORPUS_UPDATE=1 cargo test --all-features -p drt --test config_corpus

and read the diff before committing it.
