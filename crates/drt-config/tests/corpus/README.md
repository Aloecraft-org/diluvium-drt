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
`examples__21-wireguard__hub.json` is `examples/21-wireguard/hub.json`.
The name carries the provenance so the corpus needs no manifest, and
`tests/corpus.rs` fails if a copy has drifted from its source or if a
shipped shape has no copy at all.

Everything here is JSON. It used to be `*.host.lua`, which doubled as the
rule for what counted as a shape: a new one had a copy here or the
completeness test failed. `.json` cannot do that job on its own — every
example directory has a `meta.json` that is not a config — so membership
is now two lists in `tests/corpus.rs`, `SHIPPED_EXACT` and `NOT_COVERED`,
with a third test that fails when a config is in neither. `NOT_COVERED`
is where the old suffix rule's blind spot became visible: twenty-two
example configs it never reached, listed rather than left implicit.

A filename with no `__` is a shape captured from a deployment *outside*
this repository. There are two, both what `wg.sh` writes on a customer
machine: `wg.json`, kernel mode, and `wg-userspace.json`, the
`--userspace --forward` output, which is the no-root mode of #25 §3 and
a shape no example here exercises. `wg.sh` is discofetch's
(`deploy/cloud1/www/html/wg.sh`, served at `https://discofetch.net/wg.sh`)
and in neither of the repositories this corpus is shared by, so both files
were produced by *running* the script at `126b409` against the v0.6.0rc1
binary, `wg check: ok` on that run, with only the `_this` path edited.
Their own headers say so. Until 2026-09-12 `wg.json` was a
reconstruction from `doc/WireGuard.md` §1, and the reconstruction got the
`wireguard` block right and everything around it wrong: the real program
is `wg_rendezvous.dlua`, the caps and connectors carry `rest` and `fs`,
and there is no `peers` list, because the rendezvous room supplies the
peer at run time. That is the argument for a captured corpus over a
reconstructed one.

**`wg.sh` writes JSON**, since discofetch `126b409`. Before that it wrote
`.host.lua`, which stopped loading when that format was dropped, and
0.6.0rc1 records it as a known issue; these two files close it.

## The two checks

- `crates/drt-config/tests/corpus.rs` — the corpus is complete and
  verbatim. No loader involved, so it runs in any build.
- `crates/drt/tests/config_corpus.rs` — each file loads, and parses to the
  structure it parsed to before. The snapshots are in `snapshots/`,
  written on first run and compared after. That test needs the loader,
  which lives in the `drt` crate, which is why it is not in this crate
  beside the files it reads.

## Adding to it

Ship a new config under `examples/` and `corpus.rs` fails until you name
it — in `SHIPPED_EXACT` with a copy here, or in `NOT_COVERED`, which says
out loud that no gate watches it. Prefer the copy. Regenerate a snapshot
only when you meant to change what a config parses to:

    DRT_CORPUS_UPDATE=1 cargo test -p drt --test config_corpus

and read the diff before committing it. `--all-features` is no longer
needed: nothing in the JSON path is feature-gated, so every file is read
in every build and the snapshots mean the same thing everywhere.
