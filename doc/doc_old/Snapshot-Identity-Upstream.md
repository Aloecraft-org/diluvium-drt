# A snapshot does not carry key identities, so `pairs` order changes on restore

**Audience: whoever files this against `Aloecraft-org/diluvium`.** A DRT
document about the core, written here because DRT is where it was
measured and DRT hibernates instances as a matter of course. The
reproduction needs only `dv_snapshot` and `dv_restore`.

**Status: open.** diluvium 0.17.1 (`d8497b0d`) carries it. Found
2026-09-25 by DRT's determinism corpus (`tests/determinism/README.md`),
pinned by `pairs_order_across_a_hibernation_is_pinned_as_the_core_has_it`
in `crates/drt-swarm/tests/swarm.rs`.

---

## The bug in one paragraph

0.17.0 made `pairs` order over table, function, userdata and coroutine
keys a property of the program: such a key hashes by `keyid`, an identity
taken from the per-state counter `g->keyidcount` when the object is made
(`ltable.c`, `lstate.c`). `dsnap.c` never reads or writes either. A
restore therefore rebuilds every object with a fresh identity from a
fresh counter, and every table keyed by a reference is rehashed under
new identities. A program that hibernates and wakes iterates the same,
untouched table in a different order, and the objects it makes after
waking get identities offset from the ones it would have had.

Measured through DRT, one program run twice, resident and hibernated:

| report | resident | hibernated and woken |
|---|---|---|
| a table of 12 table keys, before sleeping | `6 7 8 9 10 11 12 1 2 3 4 5` | same |
| the same table, after waking | `6 7 8 9 10 11 12 1 2 3 4 5` | `12 1 2 3 4 5 6 7 8 9 10 11` |
| 12 function keys made after waking | `2 3 … 12 1` | `1 2 … 12` |
| `tostring({})` after that | `table: #178` | `table: #195` |

## What a fix has to carry

- **Each object's `keyid`**, written with the object and restored into it,
  so a restored table rehashes to the order it had.
- **`keyidcount`**, so the next object after a restore continues the
  sequence the program would have seen without a snapshot.
- Objects the restore itself makes (permanents, the stdlib) must not
  consume identities from the program's sequence, or the offset returns.

This is a snapshot-format change, so it wants the format's version bump.

## Two related asks, smaller

- **Identities depend on the build's features.** The counter includes
  everything setup creates, and `numeric` creates more, so the same
  program iterates reference keys differently on a build with `numeric`
  (`full`, `web`) than on one without (`slim`, `wasi`): a rotation of the
  same sequence, measured by `tests/determinism/03-reproducible` against
  both. Starting the program's identities at a fixed base after setup
  would make the order independent of which modules a build carries.
- **`json` loses `-0.0`.** `json.encode(-0.0)` writes `-0` (an integral
  float is written without a fraction), and `-0` decodes as the integer
  `0`: sign and subtype both lost. `msgpack` keeps both. Deterministic on
  every target, so a fidelity bug rather than a determinism one; pinned
  in `tests/determinism/02-exact`.
