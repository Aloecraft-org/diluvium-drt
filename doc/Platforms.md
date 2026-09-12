# Platforms: what each target has, and what runs on it

**Status:** the matrix, written 2026-09-03 at v0.4.2. Each cell is marked
by how it was established: **measured** (run on this tree, and the doc
that records the run is named), **decided** (a design choice recorded
elsewhere), or **expected** (a claim from platform knowledge that nobody
has run here yet). An expected cell is the first thing to measure before
building on it. `doc/Wasm.md` §3 is the source for the wasip2 and browser
columns and is more detailed on the leaf adapters; this table exists so
the comparison is in one place and covers the rows that document did
not: spawning, plugins, connectors and verbs.

| | native linux, macOS | native Windows | wasip2, under wasmtime | browser, `drt-web` |
|---|---|---|---|---|
| **status** | released: linux static x86_64, darwin arm64 and x86_64 (`doc/Release.md`) | `slim`, built and gated: cross-built from Linux with mingw-w64, run on a Windows runner through the examples gate (`drt_windows_x86_64_slim.exe`; `drt_slim_windows_x86_64.exe` from v0.5.0rc4 to v0.6.0-rc.2); `full` blocked on `exec` (unix-only) and on cross-compiling `aws-lc-sys` through russh | released: `drt_wasi.wasm` (`drt_wasip2.wasm` before v0.6.0-rc.2), gated through the examples in CI (M1, M6) | released: `drt_web.tar.gz`, gated in Chromium (M4) |
| **threads** | yes | yes, measured (`listen`'s thread per connection, example 17) | **no**, measured: `thread::spawn` is `Unsupported` | no |
| **blocking sleep** | `thread::sleep` | `thread::sleep`, measured | `thread::sleep` works, measured | **impossible** on the thread; the driver returns what it waits for and the page sleeps (D6) |
| **wall clock, monotonic** | `std::time` | `std::time`, measured | `std::time` over wasi clocks, measured | `Date.now`, `performance.now` via `web-time` |
| **entropy** | `getrandom` | `getrandom`, expected | wasi random, measured | `crypto.getRandomValues` via the `wasm_js` cfg |
| **files** | `std::fs` | `std::fs`, measured (example 04, the jail included) | `std::fs` over preopens (`--dir`), measured; the fs jail composes with wasmtime's own | a `MemFs` the page seeds and drains |
| **sockets, listen** | `std::net` plus a thread per connection (`listen`) | same code, measured (example 17) | `std::net` non-blocking, one state machine per connection polled from the drive loop (M6); needs `-S tcp=y -S inherit-network=y` | none |
| **sockets, dial** | `std::net`, tokio | expected | `std::net` non-blocking, same flags; name lookup needs its own flag | `fetch` and `WebSocket` only, async |
| **instance spawn** (`host.spawn`, the swarm) | yes | yes, expected | **yes**, measured: `08-spawn-and-hibernation` passes under wasmtime | **yes**, measured: `08` passes in Chromium |
| **process spawn** (`exec/run`, `socketpair` plugins) | yes | yes, expected, with no fd 3: spawn and dial back over loopback (`doc/Plugins.md` §4) | **no**: WASI has no process API and none is on the standardization track; a native launcher plugin restores it (`doc/Plugins.md` §4.5) | no |
| **tokio** | full | full, expected | `sync`, `macros`, `io-util`, `rt`, `time` only, measured | the same five |
| **connectors** | `full`: time, fs, crypto, sql, ssh, rest, ssmtp, exec, listen (plus `cli`, the line editor); `slim`: time, fs, crypto, listen | `slim`'s set expected; `sql` expected; the tokio-backed three depend on the `aws-lc-sys` build | `wasi`: time, fs, crypto, sql, listen | `web`: time, fs, crypto |
| **verbs** | run, start, repl, buildinfo, ps stub, relay, stun, tunnel, netcheck | the same, expected, once built | run, start, repl, buildinfo, ps stub | run, repl, start without listeners, through the terminal contract |
| **plugin transports** (`doc/Plugins.md` §4.1) | `socketpair`, `spawn` with dial-back, `tcp` | `spawn` with dial-back, `tcp` | `tcp` only; spawning through a launcher plugin | WebSocket and Worker, later |
| **`exec/run`** | builtin, `full` only, announced when wired | builtin once built | served by a native launcher plugin, never in the module | never |
| **the C core** | linked, `cc` | linked; `diluvium-sys` calls `$CC` directly, so a mingw or MSVC compiler for the target is the unknown | linked, wasi-sdk, `-W exceptions=y` at run time | linked, wasi-sdk plus wasi-libc, seventeen syscalls defined in the module (D4) |
| **the drive loop** | ticks, and the loop sleeps what a tick asks for (M3) | same | ticks; the deferred pump parks a slow connector (M3) | ticks from `setTimeout`; nothing may block |
| **the gate** | fmt, clippy, both test profiles, the examples gate (18 of 19) | none yet | the examples gate through wasmtime: 8, `08` and the served fetchpoint among them | the examples in Chromium, the REPL parity transcript, and a real xterm.js typed into |

## Reading the columns

**Native Windows was a build question, and it is answered for `slim`.**
Every row it needs exists in Rust's std on Windows; the two unknowns were
the C core's compiler and `aws-lc-sys`. The first is mingw-w64, which
diluvium-sys derives from the target triple on a Linux host and refuses
MSVC for by name, and the `x86_64-pc-windows-gnu` cross-build rehearsed
in `release.yml`'s dispatch mode the day it was tried. The rehearsal found
one platform fact the table has no row for: the C runtime opens its
streams in text mode and rewrote the C core's `\n` as `\r\n` while
Rust's `println!` did not, so a program's output was half one line
ending and half the other. `stdio::bytes_as_written` puts fds 1 and 2 in
binary mode once at startup, and the examples gate passes byte for byte
on a Windows runner -- which is the gate that proves the artifact before
it is uploaded (`smoke-windows`). So Windows gets a native `drt` that
spawns, serves and reads files, and wasmtime is not needed there. `full`
stays blocked: `exec` is unix-only by `compile_error!`, and `aws-lc-sys`
through russh is the linux aarch64 problem again.

**WireGuard is a privilege question, not a platform one.** `drt wg` and
the `wireguard` block build and run wherever `full` does, and the
`slim,wireguard` cross-build for `x86_64-pc-windows-gnu` is clean too --
what they need is not a platform feature but permission to create a
tunnel interface: CAP_NET_ADMIN or root on Linux, root on macOS,
`wintun.dll` beside the binary on Windows -- in kernel mode. In
`mode = "userspace"` there is no interface and no privilege at all: the
stack is inside the process, reached through a port on localhost, and
`wintun.dll` is not needed for it. The wasm targets are the real no:
neither has threads to drive a device. `doc/WireGuard.md` has the
measurements, including the mode's.

**wasip2 is a sandbox question.** The module cannot spawn or load
anything at run time, which is the property that makes it a
strong-isolation tier: an untrusted program runs inside wasmtime, and
every capability it reaches is served by something the operator started
and the config named. With the `tcp` plugin transport and a native
launcher, that composition gives a wasm `drt` the full connector set
without porting the tokio-backed connectors to `wasi:http` or
`wasi:sockets`.

**The browser is a page.** It has instance spawning and nothing that
leaves the tab; its capabilities are what the page supplies.

## Standards, as of September 2026

Recorded so nobody waits for the wrong thing. WASI 0.3.0 was released in
June 2026 and its headline is native async: `stream`, `future`, `async
func`. `wasi:thread-spawn` is on the standardization track. No process
spawning proposal is; the wasip2 column's "no" is not a wasmtime
limitation but the absence of an interface, and the launcher plugin is
the answer rather than a wait.
