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
| [`crates/drt-platform`](crates/drt-platform) | The leaf adapters: clock, entropy, the fs backend (a disk, or a page's memory) and stdio, `cfg`-gated per target so nothing above them is. See [`doc/Wasm.md`](doc/Wasm.md). |
| [`crates/drt-connector`](crates/drt-connector) | The `Connector` trait, registry, capability gating, and the dispatcher that guarantees every drained request is answered. Mocks implement the same trait; guests cannot tell. |
| [`crates/drt-swarm`](crates/drt-swarm) | The swarm: `dvs.c` semantics ported over the `Engine` seam (instance table, attenuated caps with provenance, lifecycle drain, budgets, hibernation + `wake_on_message`); the snapshot store; endpoint refs. |
| [`crates/drt`](crates/drt) | The binary: `run` \| `start` \| `repl` \| `relay` \| `tunnel` \| `ps` — see SPEC.md §13a. |
| [`crates/drt-web`](crates/drt-web) | The browser tier: the same `drt`, C core linked in, behind a terminal contract a page attaches xterm.js to. See [`doc/Browser.md`](doc/Browser.md). |
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

One static binary, no runtime dependencies. Download it, `chmod +x`, go:

```sh
# linux x86_64 — also drt_darwin_arm64, drt_darwin_x86_64
BASE=https://github.com/Aloecraft-org/diluvium-drt/releases/latest/download
curl -fLO $BASE/drt_linux_static_x86_64
curl -fLO $BASE/SHA256SUMS.txt
sha256sum --ignore-missing -c SHA256SUMS.txt      # shasum -a 256 -c on macOS
chmod +x drt_linux_static_x86_64 && ./drt_linux_static_x86_64 --version
```

Or let the script do it, which is the same download plus the checksum check
and a `PATH` note:

```sh
curl -fsSL $BASE/install.sh | sh
```

`DRT_SLIM=1` takes the size profile, `DRT_VERSION=vX.Y.Z` pins a release,
`DRT_PREFIX=` chooses the directory, and `DRT_MIRROR=` picks a different
source — including a directory you already have, which is how this installs
with no network at all:

```sh
DRT_MIRROR=file:///mnt/xfer/drt DRT_VERSION=v0.3.0 sh install.sh
```

Every release carries `BUILDINFO.txt` and `SHA256SUMS.txt`. `BUILDINFO`
names the diluvium revision inside the binary and the dv ABI it speaks —
`drt buildinfo` asks the binary itself, so "which diluvium is in here" is
read off the artifact rather than inferred from a tag. See
[`doc/Release.md`](doc/Release.md).

The mirror at `https://diluvium.aloecraft.org/release/drt/` is the intended
front door and the one `install.sh` prefers, but it does not carry the `drt`
namespace yet — the URLs above are the ones that resolve today.

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
C core. The wasm targets link the C core in — `drt` itself builds for
`wasm32-wasip2` and runs under wasmtime, and `drt-web` is the same runtime
for a page, where the examples pass through an in-page shell in Chromium;
the plan and the recipes for both are [`doc/Wasm.md`](doc/Wasm.md).

## Writing a program

A program reaches the host through the **`host` library**, and the smallest
useful one is a line:

```lua
print(host.time())        -- drt run hello.dlua
```

`host` is where anything that costs a grant lives — the clock, entropy, the
filesystem, sql, spawning:

```lua
host.time()                          -- wall-clock ms
host.monotonic()                     -- ms for intervals, this process's epoch
host.crypto.random(16)               -- hex
host.fs.read("note.txt")             -- raises on non-ok, with the refusal's own words
host.fs.try_read("note.txt")         -- value, status, detail — a denial is an answer
host.call("sql/exec", {sql = "..."}) -- any connector by name
host.try("sql/exec", {sql = "..."})  -- the same, without the raise
```

**`time.now()` does not exist, and the error is misleading.** `time` *is* a
library — the pure calendar one, `time.iso` / `time.parse` / `time.fields` —
so `time.now()` answers `attempt to call a nil value (field 'now')`, which
reads as a broken runtime rather than a wrong name. The clock is not there
because a clock is not pure: it costs a grant, a connector answers it, and
the answer lands in the log so a replay replays the same moment. That is
`host.time()`.

[`examples/hello.dlua`](examples/hello.dlua) is one deployment seen from
inside, run it two ways and compare;
[`examples/12-under-the-hood`](examples/12-under-the-hood) makes two of these
calls twice — by hand on the raw `host/calls` queue pair, then through the
library — which is what every call above does underneath.
[`doc/HostBaseline.md`](doc/HostBaseline.md) says which of these every DRT host
must answer — the browser tier included — and the rules that make an absent one
safe rather than mysterious.

## License

Apache-2.0, same as diluvium.
