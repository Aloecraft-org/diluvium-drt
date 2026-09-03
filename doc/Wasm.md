# The wasm port: one runtime, three targets

**Status:** the plan, written 2026-09-03 at v0.4.1 on `main`. Everything
in §2 was *run* in the session that wrote this, on this tree, with the
toolchain versions named there — none of it is inferred from a document.
The decisions in §1 rest on those measurements; where a decision rests on
a claim that was not measured, the text says so.

**What this answers.** Two asks, both from the owner:

1. A rock-solid `wasm32-unknown-unknown` build that plugs into xterm.js and
   gives, in a page, as close to the native `drt` experience as a browser
   can give.
2. A `wasm32-wasip2` build that is a completely portable, wasmtime-compatible
   `drt`.

And one background fact that shaped both: the Lab has not moved since
`diluvium-host` became DRT, because DRT's wasm story was not ready for it to
move to. `doc/Browser.md` designed a browser tier and `doc/Next.md` §7
sized it, and the honest summary of both is that wasm has been a second
tier — a separate crate, a different engine path, a JS host in the middle,
and nothing in the release. This document is the plan that makes it the
same runtime.

**Read `doc/Browser.md`, `doc/HostBaseline.md` and `doc/Next.md` §7 first**
if you have not; this supersedes the first's architecture and the third's
sizing, and keeps everything in the second.

---

## 1. The decisions

**D1. One module per target, with the C core linked in.** The `Engine` on
every target is `DiluviumEngine` over `diluvium-sys`, the same one the
native binary uses. The browser tier does *not* bridge to a JS-hosted
interpreter. `doc/Browser.md`'s JS-host-in-the-middle shape was SPEC.md
§4's *fallback* for the case where same-module linking failed; the upstream
spike answered that it does not fail (`bindings/rust/WASM-SPIKE.md`), and
§2.3 below shows the C core running inside a wasm-bindgen module in
Chromium, on this tree, today. The fallback is no longer needed, and
keeping it would keep the browser a second engine path — the exact thing
"there is never a second runtime with different behavior for the same
program" (README) forbids.

**D2. The wasip2 artifact is `drt` itself.** Not a library, not a subset
crate: `crates/drt`, the CLI, built for `wasm32-wasip2` with the connectors
that cross (§2.1: `time`, `fs`, `crypto`, `sql` all build; `listen` builds
and needs one rewrite). It runs under `wasmtime run --dir . drt.wasm run
app.dlua` and prints what the native binary prints. The examples gate is
the conformance test, run through wasmtime. A new cargo profile, `wasi`,
names the connector set, and `drt buildinfo` reports it from inside
wasmtime, so BUILDINFO gains `profile.wasi.connectors` under the same
say-what-you-carry rule as `full` and `slim`.

**D3. The browser artifact is the same `drt` library behind a
terminal-shaped API.** `crates/drt-web` becomes the wasm-bindgen cdylib
over `crates/drt`'s library (`run`, `repl`, `start` without listeners), not
over `drt-swarm` alone. Its JS surface is what a terminal needs — a command
line in, bytes out, a step to call — so xterm.js is the terminal and the
page is a shell that runs `drt run app.dlua` through the *same clap
parser*, with the same `--help` and the same errors. The swarm export
table from `doc/Browser.md` (built on `claude/drt-browser-wasm-bindgen`)
survives beside it for the Lab's Instances panel.

**D4. The browser's libc is wasi-libc, with its seventeen syscalls defined
in Rust.** The C core needs a libc; the upstream spike counted 84 symbols
for an embedder to supply by hand. Measured here (§2.3): link the wasi
sysroot's `libc.a` into the `wasm32-unknown-unknown` module and the
residue is 17 `wasi_snapshot_preview1` imports; define those 17 as
`#[no_mangle]` Rust functions under wasi-libc's own import symbol names
and the module imports **nothing but wasm-bindgen's own glue**. No JS WASI
shim, no hand-written `snprintf`, and `string.format` is the real one.

**D5. Platform code lives in leaf adapters, and there are four of them.**
Clock, entropy, the fs connector's backend, and stdio. Nothing else in the
tree is platform-specific — §2 proves it by building `drt-swarm`, `drt`
and the connectors for both wasm targets with no source change — and the
four are exactly SPEC.md §3's "platform code confined to leaf adapters".
A `drt-platform` crate holds them, `cfg`-gated per target, in
`ego_platform`'s shape; whether it *is* `ego_platform` is §7's question.

**D6. The drive loop becomes a state machine, and the pump learns to
defer.** `run.rs`, `repl.rs` and `start.rs` each own a loop that calls
`std::thread::sleep` and `pollster::block_on`. Both are fine natively and
under wasmtime, and both are impossible on a browser thread. The loop is
inverted once — `tick()` returns what it is waiting for and for how long,
the host sleeps — and the hostcall pump keeps an in-flight table so a
connector that cannot answer now (a `fetch`) answers on a later tick. This
is the one real refactor in the plan, and it is the shape the Lab's
`runSlice` and `_settled` already have (`src/kernel/swarm.js`), which is
the strongest evidence it is right.

**D7. Verification is the release trigger, and it is measured, not
smoked.** `doc/Release.md` keeps wasm32 out of the matrix until the
artifact can be verified the way the others are. The bar here: the wasip2
leg runs the examples gate under wasmtime; the browser leg runs the same
examples in Chromium and diffs `expected.txt`. `doc/Next.md` §7's point
that a node smoke test cannot see the one browser divergence this project
has hit (`doc/HostBaseline.md`) stands, and is why the browser gate is a
browser.

**D8. The REPL is one guest and one line editor.** `repl.dlua` stays the
only evaluator, and the host half — "line in, text out" plus a completion
request — is `ego-cli` on all three targets rather than a native editor
and a browser one held in step by a parity test. The editor is the same
code; only the backend under it differs, which is what makes the
behaviours enumerated in §5 identical by construction instead of by
agreement. §2.6 weighs it: +111 KB in the browser, +248 KB natively, and
tokio in a slim graph that has never had it, which is the price and the
one thing to dislike — and the one thing that got fixed, the native cost
settling at +153 KB with no runtime at all once the ask was answered. M8 is the work; its native half landed the day this
was decided, and `cli` sits in `slim` — where the argument said it
belonged — because
[ego-cli#3](https://github.com/Aloecraft-org/ego-cli/issues/3) was
answered with a runtime-free backend and
[#4](https://github.com/Aloecraft-org/ego-cli/pull/4) merged it the same
day. The smallest build has the editor and no async runtime.

*Revised 2026-09-03.* This decision read "two line editors, by contract —
natively rustyline and in the page an xterm.js readline addon, with the
behaviours enumerated in §5 so the two cannot drift silently", and §7
recorded the alternative as open. Three hand-written editors is what the
codebase actually had by then — `repl.rs`'s cooked `stdin` reader with no
editing at all, `drt-term.js`'s printable-and-backspace loop, and the
diluvium homepage's `Repl` class with arrows, `^A`/`^E`/`^U`, history and
tab completion (`diluvium/doc/repl-reference.html`) — none of which agreed
with the others. A contract between editors that do not exist yet is
cheaper to write than to keep.

The `host.time()` divergence `doc/HostBaseline.md` records
for the Lab's REPL does not exist here: under wasmtime, `drt repl` answered
`print(host.time() > 0)` with `true` (§2.2), because a REPL that is an
instance parks properly.

---

## 2. What was measured

Toolchain: rustc 1.94.1, wasi-sdk 27.0 (clang 20.1.8), wasmtime 48.0.1,
wasm-bindgen-cli 0.2.114 and 0.2.127, Chromium 141.0.7390.37 (Playwright's),
node 22. `WASI_SDK_PATH` pointed at the wasi-sdk; nothing else was set.
The diluvium pin is `515160f6` (5.5.1_build12p1), unchanged.

### 2.1 What builds, per target, with no source change

Every row is one `cargo build` on this tree. `rc` is cargo's exit status.

| crate / features | wasm32-wasip2 | wasm32-unknown-unknown | why, where it fails |
|---|---|---|---|
| `drt-swarm --no-default-features` | 0 | 0 (CI does this) | |
| `drt-swarm` (the C core, `engine-diluvium`) | 0 | 0 | `diluvium-sys` builds `onelua.c` with the wasi-sdk for both |
| `drt --no-default-features` | 0 | 0 | the whole CLI: clap, config, run, repl, start |
| `drt` + `connector-time,connector-fs,connector-crypto` | 0 | **101** | `getrandom` 0.3 wants `--cfg getrandom_backend="wasm_js"` on the browser target (§4.2) |
| `drt` (default = `slim`, so `listen` too) | 0 | — | compiles; runtime finding in §2.2 |
| `drt` + `connector-sql` | 0 | — | bundled SQLite compiles under the wasi-sdk |
| `drt` + `connector-rest` | **101** | — | tokio: "Only features sync,macros,io-util,rt,time are supported on wasm" |
| `drt` + `tunnel` | **101** | — | tokio, same sentence |
| `drt-connector-time` | — | 0 | compiles; `Instant::now` panics at runtime on this target (§4.2) |
| `drt-connector-fs` | — | 0 | compiles; every `std::fs` call answers `Unsupported` at runtime |
| `drt-connector-crypto` | — | **101** | `getrandom`, as above |
| `drt-web` | — | 0 | CI's existing job |

Two facts fall out. **Nothing above the leaf adapters is
platform-specific**: the swarm, the capability layer, the config, the
dispatcher, the CLI and the REPL all cross both targets untouched. And the
red cells are two known things, not a long tail: `getrandom`'s browser
switch, and tokio, which gates `rest`, `ssmtp`, `ssh`, `tunnel`, `relay`,
`stun` and `netcheck` off wasm entirely — those are the native-only verbs
and connectors until they are rewritten over `wasi:http` / `wasi:sockets`
(§6, later milestones), and the profile says so rather than pretending.

### 2.2 wasip2 under wasmtime, today

Built `drt` for `wasm32-wasip2` with `connector-time,connector-fs,connector-crypto,connector-sql`
and ran the component under wasmtime. `-W exceptions=y` is required: the
C core's `setjmp`/`longjmp` is lowered onto the exception-handling
proposal (`WASM-SPIKE.md`), and without the flag the module is refused at
load.

```
$ wasmtime run -W exceptions=y --dir . drt.wasm --version
drt 0.4.1
$ wasmtime run -W exceptions=y --dir . drt.wasm buildinfo
version: 0.4.1
profile: slim
dv_abi: 1
dv_abi_expected: 1
diluvium: 515160f645874fc82de001fea7f68803f47bbc58
connectors: time,fs,crypto,sql
verbs: buildinfo,ps,repl,run,start
$ wasmtime run -W exceptions=y --dir . drt.wasm run examples/hello.dlua
time:             1788401535485
fs/read note.txt:  denied no connector is wired for 'fs/read' in this process
fs/read escape:    denied no connector is wired for 'fs/read' in this process
sql/query:         denied no connector is wired for 'sql/query' in this process
$ wasmtime run -W exceptions=y --dir . drt.wasm run --config examples/deployment.json
time:             1788401535674
fs/read note.txt:  ok hello from the workspace

fs/read escape:    error '../../etc/passwd' resolves outside the granted scope; a program names files within what the host granted, and nothing beyond it
sql/query:         denied 'sql/query' is outside this instance's grants
$ printf 'x = 21\nprint(x * 2)\nprint(host.time() > 0)\n' | wasmtime run -W exceptions=y --dir . drt.wasm repl
drt repl — ^D to leave
dv> dv> 42
dv> true
dv>
```

Those are the native binary's outputs, character for character (the
clock aside). Note the second `run`: the fs jail refuses `..` on the
resolved path *inside wasmtime's own preopen*, so the two sandboxes
compose rather than fight.

**The examples gate, through wasmtime.** `examples/run-all.sh` takes a
`DRT=` binary; a three-line wrapper that `exec`s wasmtime with `--dir .`
is a `drt`:

```
$ cd examples && DRT=/path/to/drt-wasip2 ./run-all.sh
ok       01-hello                 drt run app.dlua
ok       02-capabilities          ...
ok       03-writing-dlua          drt run app.dlua
ok       04-files                 ...
ok       06-budgets               ...
ok       08-spawn-and-hibernation drt start --config app.json
ok       12-under-the-hood        drt run app.dlua
17 example(s): 7 ok, 0 failed, 10 skipped, 0 without a meta.json
```

Every example that does not need a `full`-only verb or the network passes
under wasmtime unchanged — `08` included, which is `drt start` driving a
swarm with hibernation. The ten skips are the gate's own profile logic
(`buildinfo` says `slim`), not failures.

**Sizes and startup.**

| artifact | bytes |
|---|---|
| `drt.wasm`, dev profile, `connector-sql` | 30,274,302 |
| `drt.wasm`, `release-small`, time+fs+crypto | 1,359,899 |
| `drt.wasm`, `release-small`, time+fs+crypto+sql | 2,333,889 |
| `wasmtime compile` of the latter (`.cwasm`) | 3,379,552 |

`run examples/hello.dlua` takes 0.58 s wall from the `.wasm` (Cranelift
compiling 2.3 MB on every start) and 0.010 s from the precompiled
`.cwasm`. A deployment that starts often wants the second; both are the
same bytes.

**The CPU-bound number, for calibration.** A guest loop of twenty million
iterations (`acc = acc + (i % 7)`) under wasmtime, `release-small`
(opt-level `z`): 631 ms and 641 ms on two runs. The native comparison is
§2.5.

**The listener.** `drt start` with a listener under
`wasmtime -S inherit-network=y -S tcp=y`: `TcpListener::bind` **succeeds**
— Rust's wasip2 `std::net` is real — and the next line panics:

```
thread 'main' (1) panicked at library/std/src/thread/functions.rs:131:29:
failed to spawn thread: Error { kind: Unsupported, message: "operation not supported on this platform" }
   13: drt::listen::spawn_acceptor
   14: drt::listen::bind
```

`crates/drt/src/listen.rs:213` and `:225` spawn a thread per acceptor and
per connection. wasip2 has sockets and no threads, so serving under
wasmtime is a listener rewrite (§6, M6), not a platform limit.

*Resolved at M6 (2026-09-03).* A second measurement decided the shape:
`TcpListener::set_nonblocking(true)`, `accept`, and non-blocking reads
and writes on the accepted `TcpStream` all work on wasip2 under wasmtime
48 with `-S inherit-network=y -S tcp=y`, so the rewrite needed neither
`wasi:io/poll` nor the `wasip2` crate — the polled acceptor in
`listen.rs` is `std::net` stepped from the drive loop, and
`examples/17-serving-http` serves from wasmtime through the wrapper.

### 2.3 The browser: the C core inside a wasm-bindgen module

A throwaway cdylib crate — `drt-swarm` with its default `engine-diluvium`
feature, `wasm-bindgen = "=0.2.114"`, one exported `run(src)` that loads
the source into a `DiluviumEngine` instance, runs it, pops a queue,
reads usage and takes a snapshot — built for `wasm32-unknown-unknown`
with `-C link-arg=--allow-undefined` (the flag `diluvium-sys` sets for
its own tests and a downstream binary must set itself). Three
configurations, three import inventories, read off the binary:

**(a) No libc.** 633,481 bytes. 56 imports from module `env`, which is
the C core's libc surface as the linker sees it after dead-code
elimination:

```
abort calloc clearerr clock_gettime difftime exit fclose feof ferror
fflush fgets fopen fprintf fputc fread free freopen frexp fseek ftell
fwrite getc getenv gmtime isalnum iscntrl ispunct isxdigit localeconv
localtime malloc memchr mktime realloc remove rename setlocale setvbuf
snprintf sprintf stpcpy strchr strcmp strcoll strcpy strerror strftime
strpbrk strspn strstr strtod strtoll time tolower toupper ungetc
```

(`memcpy`/`memset`/`memmove`/`memcmp`/`strlen` are absent because Rust's
`compiler_builtins` already defines them on wasm. The stdio family is
there because `lauxlib`'s file loader and `print` are linked even though
`io` and `os` are compiled out for a sealed instance.)

**(b) With wasi-libc.** Copy `share/wasi-sysroot/lib/wasm32-wasip1/libc.a`
from the wasi-sdk into `OUT_DIR` and link it. 703,518 bytes. The `env`
imports are gone; 17 remain, all from `wasi_snapshot_preview1`:

```
clock_time_get environ_get environ_sizes_get fd_close fd_fdstat_get
fd_fdstat_set_flags fd_prestat_dir_name fd_prestat_get fd_read
fd_renumber fd_seek fd_write path_open path_remove_directory path_rename
path_unlink_file proc_exit
```

**(c) With the syscalls defined in Rust.** wasi-libc declares each
syscall as an import under the C symbol
`__imported_wasi_snapshot_preview1_<name>`. Define those seventeen as
`#[no_mangle] extern "C"` functions — `fd_write` hands each iovec to a
wasm-bindgen import the page supplies; `fd_read` and the `path_*` family
answer `EBADF`/`ENOTSUP`; `clock_time_get` writes a time the page
supplies — and the linker resolves the references to the definitions:
705,795 bytes, and the import section holds **only wasm-bindgen's own
five** (`__wbindgen_describe`, `__wbindgen_describe_cast`, the two
externref table hooks, and the `__drt_write` sink). After `wasm-bindgen
--target web` the shipped `_bg.wasm` is 644,624 bytes.

**wasm-bindgen 0.2.114 refuses the module, and 0.2.127 accepts it.** The
CLI walks every function looking for `try_table`, finds the C core's
(that is what the sjlj lowering emits), decides the module uses Rust's
exception-unwinding scheme, and demands an exported global named
`__instance_terminated` (`wasm-bindgen-cli-support/src/transforms/catch_handler.rs:142-160`):

```
error: failed to generate catch wrappers
Caused by: __instance_terminated global required for catch wrappers
```

Rust on stable cannot declare a wasm global (no inline asm on wasm32),
so the workaround is one line of module-level asm in a C file compiled by
the wasi-sdk clang and passed to the link as an object (an archive
member nothing references is never pulled in):

```c
__asm__(".globaltype __instance_terminated, i32, immutable\n"
        ".globl __instance_terminated\n"
        "__instance_terminated:\n"
        ".export_name __instance_terminated, __instance_terminated\n");
```

The global's value is 0, the address wasm-bindgen's wrappers would check
for a termination flag; under rustc's stack-first layout (measured: the
module's first data segment sits at exactly 1,048,576, so bytes 0..1 MiB
are the shadow stack) that byte is written only by a stack overflow,
which traps on its own. With the workaround, 0.2.114 generates the glue.

The same crate rebuilt against **`wasm-bindgen = "=0.2.127"`** (current on
crates.io) exports `__instance_terminated` by itself — the crate now
emits it — and the 0.2.127 CLI generates the glue with no C stub. So the
pin moves to 0.2.127 and the workaround never lands in the tree. The pin
is shared with `ego_transport` and `ego_platform` (both `=0.2.114`), so a
browser build that carries transport needs the same move there; that is
an upstream ask, recorded in §7.

**In Chromium 141.** The page installs `globalThis.__drt_write` to
collect output, calls `init()`, and runs nine programs through the C
core. Every one behaved exactly as native:

| program | result | output |
|---|---|---|
| `print(1+1)` | done | `2` |
| `print(pcall(function() error('caught') end))` | done | `false	[string "spike"]:1: caught` |
| a `nil + 1` inside `pcall` | done | `attempt to perform arithmetic on a nil value (local 'x')` |
| 1000 caught errors in one chunk | done | `caught	1000` |
| declare a queue, push a table, read `info().len` | done, and the popped message is 15 bytes on the Rust side | `1` |
| `string.format('%5.2f\|%d\|%s\|%x', ...)`, `rep`, `2^53`, `floor` | done | ` 3.14\|42\|ok\|ff	abcabcabc	18	7` |
| `error('boom')`, uncaught | thrown into JS with the traceback | — |
| `print('alive', _VERSION)` afterwards | done | `alive	diluvium (lua) 5.5` |
| `queue.wait` on an empty queue | parked; `snapshot()` returned 1,215 bytes | — |

`pcall` catching a thousand times is the sjlj lowering working under
the exception-handling proposal in a real browser, inside a module that
is *also* Rust; `string.format` is wasi-libc's `snprintf`; the parked
instance snapshotting is `dv_snapshot` with the ILP32 layout. That is
the whole of the risk SPEC.md §4 named, retired on DRT's own tree.

### 2.4 What the JS-host branch established, kept

`claude/drt-browser-wasm-bindgen` (unmerged, 46 commits behind `main`)
built `doc/Browser.md`'s export layer and a seven-assertion Playwright
suite and found one thing that stays true under D1: on
`wasm32-unknown-unknown` a Rust panic is a trap, not an unwind — the
`catch_unwind` in every export never runs, the module keeps answering
afterwards with whatever invariants the panic left broken, and so **the
rule is that exports must not panic**, not that panics are caught. That
branch's `exports.rs` (the swarm table, 250 lines) and its browser-test
harness are carried into M4; its `js_bridge.rs`, and `main`'s
`bridge.rs`/`engine.rs`/`host.rs`, are the JS-host design and retire
with it.

### 2.5 The native comparison

The same twenty-million-iteration loop, timed by the guest's own
`host.monotonic()` so only the loop is measured, on the same machine:

| build | ms, two runs |
|---|---|
| native, `--release` | 358, 365 |
| wasip2, `--release`, wasmtime 48 precompiled | 640, 642 |
| wasip2, `release-small` (opt-level `z`), wasmtime 48 precompiled | 632, 654 |

wasmtime runs the C core at about 1.8× native wall time on a loop like
this, and the size profile costs nothing measurable over `--release`
there — the interpreter's hot loop is not where opt-level `z` bites. The
browser figure is not measured; V8 and SpiderMonkey compile the same
opcodes and are in the same range on this kind of code, but that is a
claim, not a number, until M4's suite prints one.

---

### 2.6 ego-cli, weighed on all three targets

`ego-cli` (Aloecraft-org/ego-cli, `3e45e91`) is line editing as a library:
an incremental ANSI decoder, a rebindable keymap, an editor with word
motions and undo, a renderer that wraps and places the cursor, history
with prefix search, and two hooks — `Completer` and `Highlighter`. It
carries four terminal backends: crossterm in raw mode natively, an
xterm.js object in the browser, cooked lines on WASI P2 (a component
cannot reach the host's termios), and a memory terminal for tests. It
pins `wasm-bindgen = "=0.2.114"`, which is the pin this tree already has.

Measured here rather than argued about, because the objection in §7 —
that `ego_platform` drags a browser kitchen sink — turns out to be true
of the object file and false of the artifact.

| what | bytes | over baseline |
|---|---|---|
| browser: glue only, after `wasm-bindgen` | 793 | — |
| browser: the editing core (decoder, keymap, editor, renderer) | 96,324 | +95 KB |
| browser: the whole `Session`, its edit loop driven, with a completer | 111,730 | +111 KB |
| native `release-small` binary: baseline | 294,512 | — |
| native `release-small`: `Session` over the real crossterm backend | 542,472 | +248 KB |
| native `release-small`: over `BlockingNative`, no runtime (ego-cli#4) | 447,384 | **+153 KB** |

The last row is what shipped, and it is the probe rather than the
estimate: measured in this tree after the pin moved, `slim` went
1,493,272 → 1,646,912 bytes, **+150 KB and +10.3%** (M8). The row above
it is what the same editor cost when it brought an async runtime with
it, kept because the difference between the two is the whole argument
for having asked.

Against what ships the *runtime* figures were **+6.4%** on
`drt_web_bg.wasm` (1,740,483 bytes) and **+16.6%** on `drt` slim
(1,490,264 bytes). For wasip2 the
answer is only that it compiles: `ego_cli`, `ego_platform` and `tokio`
all build for `wasm32-wasip2`, and the backend there is the cooked one,
which is what `drt repl` already does. (In the end `wasi` does not take
`cli` at all: it names its connectors explicitly rather than building on
`slim`, so the editor reached the native profiles and stopped there.)

**Measure the artifact, not the object.** Before `wasm-bindgen` runs, the
same browser module reads 539,085 bytes — five times the figure above —
and `strings` finds `indexedDB`, `localStorage` and `gloo` in it. None of
that survives: wasm-bindgen's `describe` functions are exported, so every
`#[wasm_bindgen]` extern in a dependency is a linker root until the CLI
removes them, and `ego_platform`'s IndexedDB blob store is reachable only
through `History::load`/`save`, which nothing calls. Anyone weighing a
browser dependency by looking at `target/wasm32-unknown-unknown/*/x.wasm`
will conclude the opposite of the truth.

What is *not* free is native. `ego_platform` takes `tokio` with `full`,
and `ego_cli`'s native backend takes `crossterm` with `event-stream`, so
a slim build that uses them has tokio in its graph — a profile that has
deliberately had none (`crates/drt/Cargo.toml`). The bytes are only paid
on use (the linker drops both when the memory terminal is the only
backend reached), but the lockfile, the build time and the audit surface
are not conditional. Both crates are already in `Cargo.lock` today behind
`ego_transport`, so `full` pays nothing new; `slim`, `wasi` and `web` do.

---

## 3. The three platforms, as facts

What each target has, so the leaf adapters in §4 are derived rather than
guessed. "std" means Rust's standard library on that target.

| | native | `wasm32-wasip2` (wasmtime) | `wasm32-unknown-unknown` (a page) |
|---|---|---|---|
| threads | yes | **no**: `thread::spawn` → `Unsupported` (measured) | no |
| blocking sleep | `thread::sleep` | `thread::sleep` works (`start`'s idle tick slept through `08`) | **impossible** on the calling thread |
| wall clock | `SystemTime` | `SystemTime` (wasi clocks; `time` answered) | `SystemTime::now` **panics**; `js_sys::Date::now` / `web-time` |
| monotonic | `Instant` | `Instant` works | `Instant::now` **panics**; `performance.now` / `web-time` |
| entropy | `getrandom` | `getrandom` (wasi random; `crypto` built) | `getrandom` needs the `wasm_js` cfg; then `crypto.getRandomValues` |
| files | `std::fs` | `std::fs` over preopens (`--dir`; `04-files` passed) | `std::fs` compiles and every call is `Unsupported` |
| sockets | `std::net` + threads | `std::net` binds (measured); no threads to serve with | none; `fetch`/`WebSocket` via JS, async only |
| tokio | full | `sync,macros,io-util,rt,time` only | same five |
| stdio | fds | fds (wasmtime's) | wasi-libc's `fd_write`, which D4 routes to the page |
| exceptions (sjlj) | native | `-W exceptions=y` at the CLI, `wasm_exceptions(true)` embedded | final EH opcodes; Chromium 137+ and comparable Firefox/Safari, per the Lab's ROADMAP §6 |
| the C core | linked | linked (wasi-sdk) | linked (wasi-sdk + wasi-libc, D4) |
| a `main` | yes | yes (`wasi:cli/run`) | **no**: exports only, driven by JS |

The last row is the one that forces D6: every other difference is a leaf
adapter, but "there is no main and nothing may block" is a property of
the drive loop.

---

## 4. The architecture, crate by crate

What changes, where, with the line the estimate rests on.

### 4.1 `drt-swarm`, `drt-caps`, `drt-config`, `drt-hostcall`, `drt-connector`: nothing

All five cross both targets untouched (§2.1). `engine.rs:208-232` already
carries `MaybeSend`/`MaybeSync` so a browser engine holding `JsValue`s is
implementable — with D1 the browser engine is `DiluviumEngine`, which is
`Send`, so the aliases become insurance rather than necessity; keep them.
`engine.rs`'s `CREATE_LOCK` mutex is a no-op on one thread and costs
nothing. The one browser-only piece is a `SnapshotStore` that is not a
directory (`snapshot.rs:12` is `std::fs`): a `MemStore` behind the same
trait for the page, with export/import through the terminal host, is
forty lines and lands in M4.

### 4.2 The four leaf adapters: `crates/drt-platform`

One small crate, `cfg`-gated per target, in the shape `ego_platform` uses
(`cfg(all(target_arch = "wasm32", target_os = "unknown"))` for the page,
`target_env = "p2"` for wasmtime, everything else native):

| seam | today | native / wasip2 | browser |
|---|---|---|---|
| `clock::wall_ms()`, `clock::monotonic_ms()` | `connectors/time/src/lib.rs:17,30,51` uses `SystemTime`/`Instant` directly; `connectors/crypto/src/lib.rs:149` uses `SystemTime` for `iat` | `std::time` | `web_time` (which is `Date.now` / `performance.now`) |
| `entropy::fill(&mut [u8])` | `connectors/crypto/src/lib.rs:506` calls `getrandom::fill` | `getrandom` | `getrandom` with `wasm_js`; the cfg goes in a workspace `.cargo/config.toml` under `[target.wasm32-unknown-unknown]`, because a build-time cfg is the only way `getrandom` 0.3 accepts it |
| `fs::Backend` | `connectors/fs/src/lib.rs:89-333` calls `std::fs` in eight places | `StdFs` | `MemFs`: a `BTreeMap<PathBuf, Vec<u8>>` with the same `canonicalize`-shaped normalisation, seeded and drained by the page |
| `stdio::write(fd, &[u8])` | the C core's `print` goes to libc `stdout` | libc's | the `fd_write` half of D4, to a JS sink |

The fs connector's jail (`canonicalize` twice, then a prefix check —
`lib.rs:89-126`) is the part that must not fork: the backend trait
carries `canonicalize`, `metadata`, `read`, `write`, `append`, `read_dir`,
`remove_file`, and the jail logic calls the trait. A `MemFs` implements
`canonicalize` as lexical normalisation over its own tree — no symlinks
exist in it, so lexical is exact there.

The three baseline families `doc/HostBaseline.md` names (`time`,
`time/monotonic`, `crypto/random`) are the first three rows. A browser
host answering them from `Date.now`, `performance.now` and
`crypto.getRandomValues` is that document's own description of a correct
host, and after this crate it is what the *same* `TimeConnector` and
`CryptoConnector` do, with no browser-specific connector at all.

`sql` is the exception among the crossing connectors: bundled SQLite
compiles for wasip2 (§2.1) and needs nothing here, but a browser build of
it would need SQLite's own libc surface over D4's mechanism plus a VFS —
real work, not in v1. The Lab's `sql.js` path keeps answering that need
in a page until then, and the browser profile says `sql` is absent.

### 4.3 `drt`: the driver, and the loop that inverts

Three loops own the same duties in three files:

| file | loop | the blocking calls |
|---|---|---|
| `run.rs:56-127` | one instance to completion | `pollster::block_on(dispatcher.dispatch(..))` at `:95`, `thread::sleep(timeout)` at `:111` |
| `repl.rs:52-125` | the REPL instance | `block_on` at `:73`, `sleep` at `:104`, and `stdin.lock().lines()` |
| `start.rs:299-355` | the swarm | `block_on` inside `PumpHost::pump` (`drt-swarm/src/pump.rs:47`), `thread::sleep` at `:353`, `bound.next_within(sleep)` at `:329` |

The refactor: one `drive.rs` with a `Driver` that owns the instance (or
the swarm) and exposes `tick(&mut self, now: Instant) -> Next`, where
`Next` is `Sleep(Duration)` (park deadline or the idle tick), `Input`
(the REPL wants a line), `Done(Outcome)` or `Failed(String)`. `run`,
`repl` and `start` become thin native loops — `match tick() { Sleep(d) =>
thread::sleep(d), Input => read a line, .. }` — and the browser calls
`tick()` from `setTimeout` with the duration it was handed. The sleep and
the clock are the host's; that is `dvs.h` doctrine restated at one level
up.

The pump stops calling `block_on`. `Dispatcher::dispatch` is already
`async`; the pump polls it once, and a `Pending` goes into an in-flight
table keyed by `(InstanceId, tok)` with the future parked beside it, to be
polled again next tick. Natively every connector answers on the first
poll — `time`, `fs`, `crypto`, `sql` do no real I/O awaiting; `ssh`,
`rest` and `ssmtp` carry their own runtimes and block inside the call —
so the native behaviour is unchanged by construction. In the page the
future is a `fetch` and completes across ticks. This is the Lab's
`_inflight`/`_settled` (`src/kernel/swarm.js`, duty 5) in Rust, and the
"deferred pump" `doc/Browser.md` says `drt-web` will eventually ship.

Two rules carry over from the Lab, verbatim: a request is not *drained*
until there is room for its reply (else a stateful connector's write is
applied twice on retry), and a deferred reply whose instance died is
dropped while one whose instance hibernated is held.

Estimate: `run.rs` 152 lines, `repl.rs` 174, `start.rs` 622 of which the
loop is ~70 — the driver is a day and a half of careful work plus a day
of tests, and it is the change most likely to reintroduce the
relay-spins-a-core class of bug (`CHANGELOG.yaml`, v0.4.0), so it lands
alone, with the `IDLE_TICK` and `next_deadline` behaviour pinned by a
test before it starts.

`listen.rs` is untouched by this milestone and stays native-only until M6
(where it grows the polled acceptor and the `Acceptor` trait this loop is
generic over).

### 4.4 `drt-web`: the browser module, second shape

`crates/drt-web` keeps its name and its `cdylib`+`rlib` layout and changes
what it is over. Contents:

- **`term.rs`** — the terminal contract, §5. `DrtTerm::new(host)`,
  `exec(argv)`, `feed(bytes)`, `tick()`. Owns a `Driver`, a `MemFs`, a
  `MemStore`, and the `Cli` from `crates/drt` — moved from `main.rs:24`
  into the library so a cdylib can reach it — parsing a command line the
  page hands it with the same clap definition the binary uses, which is
  what makes `--help` and every argument error identical.
- **`swarm.rs`** — the branch's `exports.rs`: `doc/Browser.md`'s table
  over a `Swarm<PumpHost<DeployHost>>` whose engine is `DiluviumEngine`.
  The Lab's Instances panel drives this. `handleOf(id)` goes: the
  instance is in the same module and there is no JS-side handle to map.
- **`wasi_shim.rs`** — D4's seventeen functions, in this crate rather than
  `drt-platform`, because they are `#[no_mangle]` symbols resolved by
  wasi-libc's objects at the final link and an rlib nothing references is
  a symbol the linker may never see; the final crate is where they are
  certain to be linked.
- **`platform.rs`** — the browser side of `drt-platform`'s traits over
  the host object: `now`, `random`, the fs seed/drain, the output sink.
- **`Cargo.toml`** — `wasm-bindgen = "=0.2.114"`, pinned. Not 0.2.127
  as first planned: `ego_transport` pins `=0.2.114` and a lockfile holds
  one version, so the pin stays where the workspace's is, and 0.2.114's
  one objection to this module (§2.3, `__instance_terminated`) is met by
  defining that flag in `bindings.rs` -- a `u32` the linker exports as
  the `i32` global the glue reads -- to be deleted when the pin moves.
  `js-sys`, `console_error_panic_hook`. The `.cargo/config.toml`
  rustflags for the target: `--cfg getrandom_backend="wasm_js"` and
  nothing else -- `--allow-undefined` turned out unnecessary, every
  symbol is accounted for, and a new undefined one is a link error.
- **`build.rs`** — copies `libc.a` from `WASI_SDK_PATH`'s sysroot into
  `OUT_DIR` under a name that is not `c`, and links it. The same
  `WASI_SDK_PATH` `diluvium-sys` already requires for this target, so no
  new environment variable.
- **`start`** (`bindings.rs`, `#[wasm_bindgen(start)]`) — calls
  `__wasm_call_ctors` once at load: the reactor convention by hand.
  Found the hard way: a module that carries constructors and never calls
  them gets wasm-ld's *command* treatment instead, every export wrapped
  to run the constructors before and libc's destructors after, and the
  destructor flushes stdout into the sink, which allocates a JS value
  through an export, which runs the destructors -- a stack overflow on
  the first `print`, in the first browser run of the suite.
- **`script/drt-web.sh`** — `cargo build` for the target plus
  `wasm-bindgen --target web` into `browser-test/pkg/`, refusing a CLI
  whose version is not the crate's pin; and it names
  nothing else: the C core needed a compiler wrapper here until upstream
  pinned the numeric types (§7).

Not in the crate, by the measurements: `platform.rs` (the leaf adapters
are `drt-platform`'s and the page hands over nothing but a sink) and
`swarm.rs` (the Instances panel's export table, M5).

The build produces `drt_web_bg.wasm` and `drt_web.js` (wasm-bindgen
`--target web`), and two hand-written files: `drt-term.js`, which binds
a `DrtTerm` to anything with xterm.js's `write` and `onData` -- a `$ `
prompt, a line editor, `dv> `/`>> ` when a session wants a line, `tick`
scheduling via `setTimeout` -- and `shell.js` behind it, just enough sh
to run the examples' `cmd` strings (`;`, quotes, `$?`, `echo`, `drt`).
Those two files are the CDN audience's entry point and the Lab's, and
they ship in `drt_web.tar.gz` beside the module (doc/Browser.md).

**Panic posture** (from §2.4): the `guard` wrapper stays because it is
free, and no export may rely on it. A page that catches a trap discards
the module. `console_error_panic_hook` prints the Rust message before the
trap so the reason is visible.

### 4.5 The wasip2 build: `crates/drt`, a profile, no new crate

`[features] wasi = ["connector-time", "connector-fs", "connector-crypto", "connector-sql", "listen"]`
in `crates/drt/Cargo.toml`, and `buildinfo`'s profile detector learns to
name it. `listen` joined the profile at M6, served by the polled
acceptor, with the wrapper granting the sockets. Nothing else: the CLI,
the config loader, `run`, `repl`, `start` and the swarm all ran unchanged
(§2.2).

The release leg builds `--profile release-small -p drt --no-default-features
--features wasi --target wasm32-wasip2` with `WASI_SDK_PATH` set, and its
smoke step is the examples gate through the wasmtime wrapper — not
`--version` alone, because the gate is what proves the connectors and
the C core, and it already exists.

---

## 5. The terminal contract

What `drt-web` exports for a page, and what the page must give it. The
shape is a process, because that is what xterm.js expects to be attached
to and what "the same experience as native" means.

```
  new DrtTerm(host)                 host = { write(fd, bytes), now(), monotonic(),
                                             random(bytes), files: Map<string, Uint8Array> }
  term.exec(argv: string[])         -> Session     the same argv `drt` takes: ["run", "app.dlua"],
                                                    ["repl"], ["buildinfo", "--json"], ["--help"]
  session.feed(bytes)               stdin: keystrokes, or a whole line
  session.tick()                    -> {sleepMs} | {wantsInput} | {done, status} | {failed, message}
  session.kill()
  term.files                        the MemFs, readable and writable from the page
  term.snapshots                    the MemStore, likewise
```

`tick()` is D6's `Next`, marshalled. The page loop is

```js
function pump() {
  const next = session.tick();
  if (next.sleepMs !== undefined) setTimeout(pump, next.sleepMs);
  else if (next.wantsInput) { /* resume from onData */ }
  else finish(next);
}
```

which is `swarm.js`'s `runSlice` with the slice decided by the runtime
rather than a fixed 20 ms. Nothing blocks, so the loop runs equally on
the main thread and in a worker; the Lab hosts it in its existing worker
(`src/kernel/worker-kernel.js`) for the reason it moved the kernel there —
a runaway guest freezes the worker and not the tab, and **Stop** is
`terminate()` — and the baked `file://` build, which cannot start a
worker, still runs it in the page.

**The REPL's two halves.** `repl.dlua` is the evaluator on every host.
The host half is one editor — `ego-cli`, D8 — over whichever backend the
target has, and it must provide:

- a prompt `dv> ` and a continuation prompt `>> `, chosen by the guest's
  `{more = true}` answer (`repl.rs:135-150`);
- history with Up/Down, and the readline editing set (Home/End, Ctrl-A/E,
  Ctrl-K/U/W, Ctrl-C to clear the line, Ctrl-D on an empty line to leave);
- Tab completion whose candidates come from the guest — a third message
  shape on `repl/in`/`repl/out`, `{complete = prefix}` → `{candidates =
  {...}}`, which `doc/HostBaseline.md` already assigns to the guest side
  because `host.<Tab>` is `pairs(host)`, a guest question.

That last one has a shape the editor forces, and it is the right shape
anyway. `ego-cli`'s `Completer` is synchronous and takes `&self`,
deliberately: asking the guest is a round trip through the driver, and a
keystroke is not the moment to take one. So the host asks *between*
lines — one `{complete}` message after each accepted line, whose answer
is the candidate set the next Tab serves from. A REPL's namespace changes
when a line runs, which is exactly when the snapshot is refreshed.

**Where an owning `read_line` fits a loop that must not block.** D6
inverted the drive loop so no host ever sleeps with work pending, and
`ego-cli`'s `read_line().await` owns its wait — those look opposed and are
not. There is exactly one point where a host calls it: `Step::Input`,
which the driver returns only when the instance is parked on the input
queue with nothing else to run. Waiting there is not a violation of D6,
it is the case D6 exists to identify. Everywhere else the loop still
ticks:

```
loop {
  match session.tick() {
    Step::Sleep(d)         => sleep(d),                  // a timer, or thread::sleep
    Step::Input{continuing} => { prompt(continuing); feed(read_line().await) },
    Step::Exit(status)     => return status,
  }
}
```

The `.await` is a JS microtask in a page and a `block_on` natively, and
in both the instance is idle by construction while it runs.

**And the shell.** The page's terminal shows `$ ` and accepts `drt ...`
lines, which `exec` parses with the real `Cli`. So `drt run app.dlua`,
`drt run --config with-fs.json`, `drt buildinfo`, `drt --help` and `drt
frobnicate` all say in the page what they say in a shell — and the
examples gate's `cmd` strings, which are `drt run ...` for `01`–`07` and
`12` and `drt start --config` for `08`, run unchanged with each example's
directory loaded into the MemFs. `expected.txt` becomes the oracle on all
three targets, which is the strongest parity statement this plan can
make and the cheapest to keep true.

---

## 6. Milestones, sized

Each is independently valuable, lands with its own gate, and the first
two need no design decision beyond this document.

**M1 — the `wasi` profile and the wasmtime gate in CI. ~1 day. Landed
2026-09-03** (`script/drt-wasip2.sh`, the `wasip2` job in `ci.yml`, the
`build-wasip2` leg in `release.yml`, `profile: wasi` from `buildinfo`;
the gate: 7 ok, 0 failed, 10 skipped by profile). The
feature in `crates/drt/Cargo.toml`; `buildinfo` naming it; a `wasip2` job
in `ci.yml` that installs the wasi-sdk tarball (119 MB, cached by URL)
and a wasmtime release, builds `release-small`, and runs
`examples/run-all.sh` through the wrapper; the release-matrix leg with
`profile.wasi.connectors` in BUILDINFO. Everything in it was run in §2.2.
Blocked on nothing; deliverable the day it is started.

**M2 — the leaf adapters. ~2 days. Landed 2026-09-03** (`crates/drt-platform`:
`clock`, `entropy`, `fs::{Backend, StdFs, MemFs, host}`, `stdio`; the three
connectors and the loader over it; the gate build green). `crates/drt-platform`, the `time`,
`crypto` and `fs` connectors over it, `MemFs`, the getrandom cfg. Gate:
the three connectors' existing tests green natively, and
`cargo build -p drt --no-default-features --features connector-time,connector-fs,connector-crypto --target wasm32-unknown-unknown`
green (the red cell in §2.1). No behaviour change on any shipping target.

**M3 — the driver and the deferred pump. ~3-4 days. Landed 2026-09-03**
(`crates/drt/src/drive.rs`: `Solo::tick` for `run` and `repl`,
`start::DeployDriver` for the deployment; `Dispatcher::route` and
`PendingCall`; `drt_swarm::pump::Pump`, the in-flight table; `pollster` is
gone from the runtime crates; `tests/drive.rs` pins the deferred answer
and the cadence). `crates/drt/src/drive.rs`;
`run`, `repl`, `start` over it; the pump's in-flight table. Gate: every
existing test, the examples gate natively and under wasmtime, and two new
tests — a mock connector that answers on the second poll is delivered on
the next tick, and an idle deployment's tick cadence matches
`IDLE_TICK`/`next_deadline` exactly (the relay-spins-a-core regression
test). Lands alone.

**M4 — `drt-web`, second shape. ~1.5-2 weeks. Landed 2026-09-03**
(`crates/drt-web`: `term.rs`, `bindings.rs`, `wasi_shim.rs`, `build.rs`;
`crates/drt/src/cli.rs` is the binary's `main.rs` moved into the library
so a page parses the same command line; the `web` profile in
`crates/drt/Cargo.toml` and `buildinfo`; `script/drt-web.sh`;
`browser-test/` with `shell.js`, `drt-term.js`, `run.mjs`; the `browser`
job in `ci.yml`; `build-web` and `profile.web.connectors` in
`release.yml`. The gate: 01, 02, 03, 04, 06, 08, 12 and the REPL parity
check pass in Chromium 141, 10 skipped by profile or network -- the same
seven examples wasmtime passes. Two things the suite found on its first
run that nothing else would have: the command-export recursion, and the
C core's 32-bit integers, both in §4.4 and §7. The swarm exports move to
M5, where their consumer is.) §4.4 and §5: the wasi-libc
link and the syscall shim, the wasm-bindgen pin move, `DrtTerm`, the
swarm exports from the branch, `drt-term.js`, and a Playwright suite (from
the branch's harness) that runs examples `01`, `02`, `03`, `06` and `12`
through the in-page shell and diffs `expected.txt`, plus the REPL parity
script. The C core's browser behaviour is settled (§2.3); the unknowns are
in the page-side fs seeding and in xterm.js integration, which is why the
range is wide. Gate: that suite, in Chromium, in CI, with the browser leg
joining the release matrix and `profile.web` in BUILDINFO.

**M5 — the hosts. The surface here, then three consumers of it.**

This was written as "the Lab, ~1 week, in `diluvium-lab`", which named
one consumer and mistook it for the milestone. There are at least three
places an xterm.js terminal wants `drt` behind it, they want the same
surface, and only that surface is this repository's work.

**The surface** (`drt_web.tar.gz`, doc/Browser.md) is `drt_web_bg.wasm` +
`drt_web.js` + `drt-term.js` + `shell.js`, and the whole of what a host
writes is:

```js
const terminal = new Terminal({ /* the host's own theme, addons, font */ });
terminal.open(element);
await init();
const { term, run, reset } = attach(DrtTerm, terminal);
term.putFile('/hello.dlua', bytes);        // the page owns the filesystem
```

`attach` takes the host's `Terminal` object and never imports xterm.js,
so a bundler, an import map and a `<script>` tag all work; `run(line)`
submits a line nobody typed, for a panel that has buttons as well as a
keyboard, and `reset()` returns to the prompt from whatever is running.
The suite's `xterm-embedding` check holds this to the real thing rather
than to a stand-in: it types into an actual xterm.js `Terminal` in
Chromium and reads the assertion out of that terminal's own rendered
buffer.

**Host 1: the diluvium homepage panel.** It lives at `site/` in the
diluvium repository as of `f47747f`, and its terminal is three files —
`site/static/assets/repl/{terminal.js, runtime.js, wasi.js}` over a
vendored xterm.js — which is exactly the three hand-written things this
replaces outright: a WASI shim in the page (45 imports, an `ENOSYS`
`Proxy`, `fd_write` reassembling iovecs — `wasi_shim.rs`
does this inside the module, so nothing is imported); a `Repl`
class doing arrows, `^A`/`^E`/`^U`, history and Tab (D8's one editor);
and `init_lua`/`repl_eval`/`repl_complete` as the evaluation protocol
(`repl.dlua` over two queues). `diluvium/doc/repl-reference.html` is the
annotated reference the three were built from and reads as the
specification of what is being replaced. What the panel gains is everything else
`drt` is: `drt run`, `drt buildinfo`, capabilities, a filesystem, and
`--help` that matches the binary's.

What it must decide first is **sealed or not**, and it is the same
question §7 asks about the Lab's cells, arriving here first because the
homepage is a *language* demo. That page's REPL is unsealed — `os`, `io`
and `require` are present, state persists across lines — and `drt repl`
is sealed by design, so a homepage that swapped one for the other would
quietly stop being able to demonstrate the language. Either the panel
runs a `drt` verb that hosts an unsealed evaluator (the guest already
supports it: `LoadSpec::unsafe_stdlib`, refused for `drt run` on
purpose), or the homepage keeps the language kernel for its REPL tab and
uses `drt` for a second one. Not this plan's call, but this plan should
not pretend the swap is free.

Upstream moved on this while M5 was being written: `b09e825` exposes
`diluvium_repl_load` and `diluvium_repl_complete` to Rust behind a
diluvium-sys `lua` feature, with `luaL_checkversion` as the guard. That
is the unsealed evaluator reachable from Rust rather than only from a
page's JS, which makes the first option buildable — and it is the same
`drepl.c` the homepage calls today, so a `drt` verb over it would answer
"keep typing" and complete names identically. It is deliberately off by
default, "the same kind of statement `DV_FLAG_UNSAFE_STDLIB` makes".

**Host 2: the Lab** (`diluvium-lab`, ~1 week). A **Terminal** tool in the
rail (`src/notebook/panel.js` registration, xterm.js vendored) over
`drt_web_bg.wasm` in the kernel worker; the runtime registry learning a
second artifact namespace (`releases.json` under `/release/drt/`, the
mirror ask in `doc/Release.md`, with the same three-source checksum
cross-check); the Instances panel over the `Swarm` exports instead of
`diluvium_swarm_wasi.wasm`'s `dvs_*` — which is the Lab exiting the
`dvs.c` dependency `doc/Browser.md` names, and what lets upstream delete
`dvs.c` on its own schedule. The notebook cells stay on
`libdiluvium_wasi.wasm`: they are the language kernel, unsealed and
stateful across cells, which is a different product from `drt run`; §7
records the question of whether that should change.

**Host 3: whoever else.** The tarball is a release artifact and the five
lines above are its documentation, so a p2p web app embedding a sealed
runtime is not a fourth integration — it is the same one. This is the
audience `doc/Browser.md` calls the CDN audience, and the reason `attach`
is duck-typed and imports nothing.

**What the surface still owes each of them**, and therefore what belongs
in this repository: the `Swarm` exports (host 2 — the only piece of the
old M5 that was really ours); completion candidates from the guest (§5,
all three); and the sealed/unsealed verb (host 1). Everything else on
this list is a consumer's own work against a surface that now exists.

**M6 — `listen` on wasip2. ~2-3 days. Landed 2026-09-03**
(`crates/drt/src/listen.rs`: the bridge's parsing and response bytes
made pure and shared, an `Acceptor` trait the serve loop is generic
over, the threaded acceptor natively and a `polled` one — non-blocking
`std::net`, one state machine per connection, stepped every `POLL_TICK`
from the drive loop — on wasi, compiled and tested natively too; `listen`
in the `wasi` profile; the wrapper grants `-S inherit-network=y -S
tcp=y`; `examples/17-serving-http` is the gate's served example and runs
natively and under wasmtime, skipped by name in the browser). Planned as
`wasi:sockets` and `wasi:io/poll` directly; the second measurement in
§2.2 showed `std::net`'s non-blocking sockets already work there, so the
`wasip2` crate stayed out. A poll-based acceptor over
`wasi:sockets` and `wasi:io/poll` (the `wasip2` crate `ego_transport`
already carries) behind `cfg(target_env = "p2")`, folded into the driver's
idle wait the way `bound.next_within` is natively. `std::net` binds on
wasip2 today (§2.2) but exposes no pollable, so the wasi API is used
directly there. Gate: `examples/08` and a listener example served under
`wasmtime -S tcp=y`, curl'd from the runner. After this, `drt start`
serves a fetchpoint from wasmtime, which is the "completely portable"
half of the ask.

**M7 — retire the JS-host bridge. Landed 2026-09-03, with M4** (the
four files deleted in the same change that replaced them, since the
crate could not carry both shapes; `doc/Browser.md` rewritten to the
terminal contract). Delete `bridge.rs`, `engine.rs`,
`host.rs`, `tests/bridge.rs` and the branch's `js_bridge.rs`; rewrite
`doc/Browser.md` to the terminal contract and the export table; close the
branch. Zero risk once M4's suite is green, and not before.

**M8 — one line editor, `ego-cli`'s. ~3-4 days. Native half landed
2026-09-03** (`cli` in `crates/drt/Cargo.toml`, in `full` and not in
`slim`; `repl.rs` split into `piped` — a pipe, prompts on stderr, byte
for byte what it was — and `edited`, the tty, over `ego_cli::Session`;
`Repl::names` and the `{complete}` message in `repl.dlua`, so Tab's
candidates are the names the running instance answered with;
`tests/editor.rs` scripts five behaviours through `MemTerminal`. `drt
repl` on a tty now has history, word motions, undo and Tab; it had none
of them. **Still to do:** `cli` into `slim` once ego-cli#3's branch
lands on `main` (below), and the browser half — `drt-web` keeps
`drt-term.js`'s printable-and-backspace loop until
`XtermTerminal::attach` replaces it.)

*The plan as written:* D8, §2.6. `ego_cli` in
`crates/drt` behind a `cli` feature, with `Session` driven at the one
seam §5 identifies (`Step::Input`, where the instance is parked and the
host has nothing else to do): `repl.rs`'s native half becomes the
crossterm backend, `drt-web`'s becomes `XtermTerminal::attach` over the
object `drt-term.js` already receives, and the wasip2 build takes the
cooked backend it already behaves like. `drt repl` gets history, word
motions, undo and Tab; the browser gets the same ones rather than
`drt-term.js`'s printable-and-backspace; and the completion snapshot in
§5 is what Tab serves from. The gate: the browser suite's REPL parity
check unchanged (it diffs the page against the native transcript, so one
editor must produce both), plus a keystroke script through
`MemTerminal` asserting the editing set on every target, plus the size
budget from §2.6 held to within 10%.

The awkward part is the one to decide before starting rather than
during: `slim` gains tokio and crossterm (§2.6). Three ways out, in the
order they should be tried — an upstream ask for an `ego-cli` native
backend over blocking `crossterm::event::poll`, which needs no runtime
at all and suits a tick loop better than `EventStream` does; or the
`cli` feature simply not being in `slim`, which keeps the byte count
honest but leaves the *native* terminal — the one a human actually types
at — as the only one without the editor, which is backwards; or accepting
tokio in slim and saying so in the manifest where the comment currently
promises otherwise.

*Taken, 2026-09-03: the first, and it landed the same day, so the interim
lasted hours rather than weeks.* The ask was
[ego-cli#3](https://github.com/Aloecraft-org/ego-cli/issues/3). While it
was open `cli` rode `full` — which carries tokio already, for the relay
and the STUN server — so `slim` kept its promise and the smallest build
was the one still without an editor: the backwards half of the second
option, taken deliberately and meant to be temporary. It was.

*Answered upstream the same day, and measured here.* ego-cli#3 came back
with a `runtime` feature (default on) and a `term::blocking::BlockingNative`
that needs no executor, on `claude/ego-cli-cross-platform-7m8d6v`. Three
corrections to the sketch this document had: `ego_platform::io::Stdout`
cannot stay on the write side — it wraps tokio's, which panics with no
runtime in scope, so the blocking backend writes with `std::io::Write`;
a second type rather than a feature that redefines `NativeTerminal`,
because a feature unifies across the build graph and would decide for
every crate in it at once; and `ego_platform` itself has to become
optional, since it is a hard dependency and takes tokio with `full` —
gating `History::load`/`save` is the only user-visible loss, and
`encode`/`decode` stay pure.

Tried against this tree with the dependency pointed at that branch and
`default-features = false`, and `drt`'s `cli` feature taking
`futures-executor` in place of `dep:tokio`:

| | crates | `release-small` |
|---|---|---|
| `slim` | 76 | 1,493,464 |
| `slim` + `cli`, runtime-free | 108 | 1,646,400 |

**+153 KB, +10.2%, and `cargo tree -i tokio` finds nothing** — where the
runtime backend cost +248 KB and an async runtime. All five of
`tests/editor.rs` pass over it unchanged.

*Landed 2026-09-03, the same day again.*
[ego-cli#4](https://github.com/Aloecraft-org/ego-cli/pull/4) merged that
branch to `main` unchanged — the code measured above is what shipped,
plus nine lines of module doc recording the feature-unification
argument. So the pin moved to `e5bd70d` with `default-features = false`,
and `cli` is in `slim`, and so in every profile that carries a native
terminal:

| | crates | `release-small` |
|---|---|---|
| `slim` without `cli` | 59 | 1,493,272 |
| `slim` | 89 | 1,646,912 |

**+150 KB, +10.3%**, re-measured against the merge rather than the
branch, and `cargo tree -e normal -i tokio` prints "nothing to print"
for `slim`. `ego_platform` left `Cargo.lock` entirely.

Two things fell out that the estimate did not have. `edited()` no longer
builds a tokio runtime at all — `futures_executor::block_on` is the
whole executor, because a `Session` over `BlockingNative` never returns
`Pending` — which also deletes the `Box::leak` that worked around tokio
1.53.1's teardown use-after-free; `cli.rs` still carries that leak for
the relay, and this is one fewer place that bug is load-bearing. And
`full` now takes the blocking backend too, since the pin turns `runtime`
off for the whole workspace: one editor backend on every target rather
than two, and `full`'s tokio is the relay's and STUN's alone.

`wasi` and `web` name their connectors explicitly rather than building on
`slim`, so they are untouched — the browser half below is still the
browser half.

**Later, named so they are not mistaken for forgotten:** the plugin
channel and a `browser/*` capability over it, both assessed against M3's
pump and now unblocked by it (`doc/Plugins.md`, `doc/Playwright.md`);
`rest` over
`wasi:http` and `fetch` (the deferred pump is the prerequisite, M3);
`sql` in the browser; `ego_transport` on wasm (it already builds for both
targets with `WebSocket`/`wasip2` backends) for `webrtc://` and the
browser doing SSH — SPEC.md §13b's seam; the wasm-engine tier
(`diluvium-wasmtime`, SPEC.md §8) is a different thing and unaffected.

Where this stands (2026-09-03): M1, M2, M3, M4, M6 and M7 are landed, so
"`drt` runs in a terminal, verified in CI on all three targets" is done
and `drt_web.tar.gz` ships. What is left is M5 — which is now three
consumers of a surface that exists rather than one integration that does
not — and M8, the editor those consumers all want. Original estimate,
kept for calibration: "M1–M5, roughly four to five weeks of focused work,
with M1 and M2 shippable in the first three days and M6 a further half
week for the served-deployment story."

---

## 7. Open, deliberately

- **`drt-platform` or `ego_platform`.** `ego_platform` (already in
  `Cargo.lock` behind `ego_transport`) has `web-time`, `getrandom` with
  `js`, a browser fs over `localStorage`, an `IdbStore`, and a `Pacer` —
  everything §4.2 needs and more. It also brings tokio `full` natively,
  `getrandom` 0.2 beside DRT's 0.3, and an async-flavoured API for seams
  DRT wants synchronous. Recommendation: `drt-platform` now, in
  `ego_platform`'s shape, and fold it in when `drt` takes `ego_transport`
  on wasm — at which point `ego_platform` is in the browser build anyway.
  Reversible either way; the traits are the same four.

  *Half-answered by §2.6 (2026-09-03).* M8 brings `ego_platform` into the
  browser and wasip2 builds through `ego-cli`, and it costs nothing there
  that was feared: its IndexedDB and `localStorage` surface is stripped
  whole by `wasm-bindgen` because nothing reaches it. So the two coexist
  rather than compete — `drt-platform` stays the seam DRT's own code is
  written against, and `ego_platform` arrives underneath a dependency, as
  predicted. The native half of the objection stands unchanged: tokio
  `full`, and getrandom 0.2 beside 0.3.
- **The wasm-bindgen pin, across three repositories.** `=0.2.114`
  everywhere, because `ego_transport` and `ego_platform` pin it and one
  lockfile holds one version; `drt-web` pays for that with the
  `__instance_terminated` definition in `bindings.rs` (§2.3, §4.4).
  Upstream ask: move `ego_transport`'s pin to 0.2.127 or later, and the
  definition here is deleted the same day. Not blocking anything.
- ~~**`-DLUA_USE_C89` in diluvium-sys, for `wasm32-unknown-unknown`.**~~
  **Fixed upstream 2026-09-03, and better than the ask.** The flag dates
  from the target having no libc and made `lua_Integer` a 32-bit `long`;
  `host.time()` -- milliseconds since the epoch -- did not fit, and the
  browser suite found it on its first run (01 and 12 failing on the
  clock). The ask here was for diluvium-sys to drop the define for this
  target, and `script/drt-web-cc.sh` stripped it meanwhile. Upstream fixed
  the cause instead: `luaconf.h` now pins `long long` and `double` on
  every target (`44b60fc`, "Pin the numeric types, so a chunk loads on
  every build"), because `LUA_USE_C89` is a statement about the *library*
  a target has and was carrying a second, unrelated decision about
  numeric width. So the flag is harmless now and the browser gets 64-bit
  integers like everything else. Verified here: with the diluvium pin
  moved to `f4d5251` and the stock wasi-sdk clang -- no wrapper -- 01, 12,
  the xterm embedding and the REPL parity check all pass. The wrapper is
  deleted. Two things came with the fix that are worth knowing: chunks
  now carry a pinned header, so bytecode from an older build is refused
  by name rather than misread (DRT ships text, so nothing here changes),
  and `math.maxinteger` in a page stops being 2147483647.
- **Whether the Lab's cells should run on `drt` too.** One runtime is the
  doctrine; the notebook kernel is Lua-with-more, unsealed, and `drt run`
  is sealed by design. A `drt` verb that hosts an unsealed evaluator
  (`unsafe_stdlib: true`, the REPL guest's `load` over `_G`) would make
  it one artifact. Not this plan's call; the Lab's.
- **Persistent files in the page.** `MemFs` first. OPFS gives a real,
  persistent, synchronous-in-a-worker filesystem and is the natural
  second backend; IndexedDB is the fallback where OPFS is not. Decide when
  someone wants a notebook's files to survive a reload.
- **One line editor or two.** ~~§5 chooses rustyline plus an xterm.js
  addon with a parity test.~~ **Answered 2026-09-03: one.** The single
  Rust editor over a `Terminal` trait this entry describes as
  hypothetical — "raw bytes in, escape sequences out ... at the cost of
  writing one" — already exists as `ego-cli`, weighed in §2.6, decided in
  D8, and built in M8. Nobody has to write it, and parity becomes
  structural rather than tested. What is left open is only where the
  `cli` feature sits in the profile table, which M8 states.
- **`exec`.** Never, on any target; diluvium's `doc/DRT.md` records the
  gap as untracked, and it stays untracked here on purpose — a page and a
  wasmtime sandbox have no process to exec, and native declines it
  deliberately (`examples/10-ssh-exec`).

---

## 8. Appendix: the recipes, so nobody re-derives them

**The wasip2 build and gate**, exactly as run:

```sh
export WASI_SDK_PATH=/opt/wasi-sdk-27.0-x86_64-linux        # >= 24; 27 verified
rustup target add wasm32-wasip2
cargo build --profile release-small -p drt --no-default-features \
  --features wasi --target wasm32-wasip2
script/drt-wasip2.sh run examples/hello.dlua     # the wrapper: wasmtime, flags, --dir .
cd examples && DRT=../script/drt-wasip2.sh ./run-all.sh
```

`wasmtime compile -W exceptions=y -o drt.cwasm drt.wasm` and
`wasmtime run --allow-precompiled ... drt.cwasm` for the 10 ms start.
Serving needs `-S inherit-network=y -S tcp=y`, which the wrapper passes;
`cd examples/17-serving-http && DRT=../../script/drt-wasip2.sh ./demo.sh`
is a deployment served from wasmtime and curl'd.

**The browser build and gate**, exactly as run (the pieces below are
what the spike established and `crates/drt-web` now carries):

```sh
export WASI_SDK_PATH=/opt/wasi-sdk-27.0-x86_64-linux
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.114 --locked   # the crate's pin, exactly
script/drt-web.sh                       # -> crates/drt-web/browser-test/pkg
cd crates/drt-web/browser-test && npm ci && npx playwright install chromium && npm test
```

**The browser spike**, reduced to its three load-bearing pieces. A
`cdylib` over `drt-swarm` (default features) and `wasm-bindgen`, with
`.cargo/config.toml`:

```toml
[target.wasm32-unknown-unknown]
rustflags = ["-C", "link-arg=--allow-undefined", "--cfg", "getrandom_backend=\"wasm_js\""]
```

`build.rs`:

```rust
let sdk = std::path::PathBuf::from(std::env::var("WASI_SDK_PATH").unwrap());
let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
std::fs::copy(sdk.join("share/wasi-sysroot/lib/wasm32-wasip1/libc.a"), out.join("libwasilibc.a")).unwrap();
println!("cargo:rustc-link-search=native={}", out.display());
println!("cargo:rustc-link-lib=static=wasilibc");
```

and the seventeen, of which `fd_write` is the only one that carries data
(the rest answer `8` (`EBADF`) or `58` (`ENOTSUP`), and `clock_time_get`
writes what the page's clock says):

```rust
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = globalThis, js_name = __drt_write)]
    fn drt_write(fd: u32, bytes: &[u8]);
}

#[no_mangle]
pub extern "C" fn __imported_wasi_snapshot_preview1_fd_write(fd: i32, iov: i32, n: i32, out: i32) -> i32 {
    let mut total = 0u32;
    for i in 0..n {
        let base = unsafe { *((iov + i * 8) as *const u32) };
        let len = unsafe { *((iov + i * 8 + 4) as *const u32) };
        drt_write(fd as u32, unsafe { std::slice::from_raw_parts(base as *const u8, len as usize) });
        total += len;
    }
    unsafe { *(out as *mut u32) = total };
    0
}
// clock_time_get(i32, i64, i32) -> i32; environ_get(i32, i32); environ_sizes_get(i32, i32);
// fd_close(i32); fd_fdstat_get(i32, i32); fd_fdstat_set_flags(i32, i32);
// fd_prestat_dir_name(i32, i32, i32); fd_prestat_get(i32, i32); fd_read(i32, i32, i32, i32);
// fd_renumber(i32, i32); fd_seek(i32, i64, i32, i32);
// path_open(i32, i32, i32, i32, i32, i64, i64, i32, i32); path_remove_directory(i32, i32, i32);
// path_rename(i32, i32, i32, i32, i32, i32); path_unlink_file(i32, i32, i32); proc_exit(i32) -> !
```

Then `wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/<crate>.wasm`,
a page that sets `globalThis.__drt_write` before `await init()`, and
Playwright's Chromium to run it. Reading the import and export sections
off the binary needs no engine: sections 2 and 7 of the module, LEB128
lengths and UTF-8 names, forty lines of Python.

**The loop** behind §2.5's table, so it can be re-run on another machine:

```lua
local t = host.monotonic()
local acc = 0
for i = 1, 20000000 do acc = acc + (i % 7) end
print(acc, host.monotonic() - t)
```
