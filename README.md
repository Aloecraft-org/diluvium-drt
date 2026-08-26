# DRT — the Diluvium RunTime

The runtime environment for [Diluvium](https://github.com/Aloecraft-org/diluvium):
swarm, transparent host with root config, hostcall connectors, orchestration,
transport, observability, and the attachable REPL. DRT embeds the diluvium
language core via FFI; installing DRT gets you everything.

The split, in one paragraph: **diluvium** is purely the language — a
100%-backward-compatible extension of Lua, with the `dv.h` instance ABI as its
embedding surface. **DRT** (this repo, Rust) is everything around a running
program. The two are concentric, not parallel: there is never a second runtime
with different behavior for the same program.

Read [`SPEC.md`](SPEC.md) first — it is the founding spec and the map of this
repository. [`GUARANTEES.md`](GUARANTEES.md) says what this runtime does and
does not promise; [`doc/Hostcall.md`](doc/Hostcall.md) is the normative
hostcall encoding (moved here from diluvium).

## Workspace

| crate | what |
|---|---|
| [`crates/drt-hostcall`](crates/drt-hostcall) | The hostcall encoding as serde types: request/reply, status enum, token space. Implements `doc/Hostcall.md`. |
| [`crates/drt-caps`](crates/drt-caps) | Capability grammar: effect × capability × scope, `host:fs/*` pattern match (same semantics as `dvs_holds`), attenuation check, provenance chain. |
| [`crates/drt-config`](crates/drt-config) | Manifest/config schema. The serde types are the source of truth; one shape at every depth — host-config and spawn-request are the same object. |
| [`crates/drt-connector`](crates/drt-connector) | The `Connector` trait, registry, capability gating, and the dispatcher that guarantees every drained request is answered. Mocks implement the same trait; guests cannot tell. |
| [`crates/drt-swarm`](crates/drt-swarm) | The `dvs.c` port over `diluvium-sys`; the `Engine` seam; the snapshot store. |
| [`crates/drt`](crates/drt) | The binary: `run` \| `serve` \| `repl` \| `ps`. |
| [`connectors/`](connectors) | Connector implementations, each feature-gated: `time` today; `fs`, `sql`, `listen`, `exec`, `ssh` per SPEC.md §7. |

## Building

```
cargo build
cargo test
```

The workspace builds standalone today. The `drt-swarm` port is gated on the
completed `diluvium-sys` transcription upstream (SPEC.md §4) and consumes
`ego-proc` / `ego-transport` / `ego-platform` as they land; those are seams,
not blockers, and the crates say so where it matters.

## License

Apache-2.0, same as diluvium.
