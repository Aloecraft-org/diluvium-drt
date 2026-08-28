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
| [`crates/drt-swarm`](crates/drt-swarm) | The swarm: `dvs.c` semantics ported over the `Engine` seam (instance table, attenuated caps with provenance, lifecycle drain, budgets, hibernation + `wake_on_message`); the snapshot store; endpoint refs. |
| [`crates/drt`](crates/drt) | The binary: `run` \| `start` \| `repl` \| `relay` \| `tunnel` \| `ps` — see SPEC.md §13a. |
| [`crates/drt-web`](crates/drt-web) | The browser tier: an `Engine` over a JS host bridge, so the same swarm runs in a page. See [`doc/Browser.md`](doc/Browser.md). |
| [`connectors/`](connectors) | Connector implementations, each feature-gated: `time`, `fs` and `sql` (each a granted directory) and `ssh` (client, `host:ssh/exec`) today; `listen` and `exec` per SPEC.md §7. |

## Building

```
cargo build
cargo test
```

A C toolchain is required: the `diluvium` crates arrive as a git dependency
and `diluvium-sys` compiles the amalgamated C core from that checkout. Try
it:

```
cargo run -p drt -- run examples/hello.dlua
```

A deployment is a config plus a program. The config names the ceiling and
wires each connector to a *place*; the program names resources inside those
places, and the config never carries an application's filenames
([`examples/deployment.json`](examples/deployment.json)):

```
cargo run -p drt -- run --config examples/deployment.json
```

An ill-scoped grant or an unreachable scope fails at startup, by name —
never as a mystifying `denied` at first call.

## Installing

```
curl -fsSL https://diluvium.aloecraft.org/release/drt/install.sh | sh
```

`DRT_SLIM=1` installs the size profile; `DRT_VERSION=vX.Y.Z` pins a release.
Or take a binary straight from
[Releases](https://github.com/Aloecraft-org/diluvium-drt/releases) —
each one carries a `BUILDINFO.txt` naming the diluvium revision inside it
and the dv ABI it speaks. See [`doc/Release.md`](doc/Release.md).

## What you can do with it today

```
drt run prog.dlua                    # one program, to completion
drt repl                             # a REPL, which is an instance
drt --config app.host.lua start      # the deployment: swarm + listeners + relay
drt --config rv.host.lua relay       # the rendezvous relay, standalone
drt tunnel --park wss://…/park/xps?k=… --to 127.0.0.1:22   # the device half
ssh -o ProxyCommand="drt tunnel wss://…/s/xps?k=…" user@xps # the caller half
```

`drt start` reads a diluvium-host `.host.lua` unchanged — a deployment moves
to DRT by swapping the binary and editing no files.
[`doc/Relay.md`](doc/Relay.md) is the SSH-to-anything-from-anywhere recipe,
including the control plane a supervisor uses for presence, metering and
arbitration.

`drt ps` and REPL *attach* are still ahead: both reach a deployment running
in another process, which is the control endpoint's job and lands with sshd.

A seams-only build (`--no-default-features`) compiles the traits without the
C core — the shape the wasm targets start from.

## License

Apache-2.0, same as diluvium.
