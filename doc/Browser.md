# The browser tier: `drt-web` and the terminal contract

`crates/drt-web` is the same `drt` the binary is -- the `web` profile of
`crates/drt` (`time`, `fs`, `crypto`), the C core and wasi-libc linked
into one `wasm32-unknown-unknown` module -- behind a contract a page
attaches a terminal to. `doc/Wasm.md` is the plan and the measurements
that chose this shape (D3, D4, §2.3, §5); this file is the contract, for
whoever writes the page: the Lab, or anyone loading `drt_web.tar.gz` from
a release.

**Status (2026-09-03, doc/Wasm.md M4):** shipped. The examples gate runs
in Chromium through the in-page shell and diffs the same `expected.txt`
the native binary and the wasmtime build are diffed against, and the REPL
typed at `drt-term.js` matches the native `drt repl` byte for byte
(`crates/drt-web/browser-test`, the `browser` job in `ci.yml`, the
`build-web` leg in `release.yml`). The JS-host bridge this document once
specified -- a swarm in wasm calling out to a JS-hosted interpreter -- is
gone (M7); it was SPEC.md §4's fallback for the case where the C core
could not be linked into the same module, and it can.

## The shape

```
  page ──── exec(argv) ────>  DrtTerm  ──── the same Cli, assemble, Solo/Repl/DeployDriver
       <─── sink(fd, bytes) ─   │              the native loops drive (crates/drt)
       <─── tick() ──────────   └── MemFs the page seeds; web-time; getrandom; wasi-libc
```

One module, no second runtime. `exec` parses the command line with the
binary's own clap definition, assembles the same config and dispatcher,
and prepares the same driver `drt run`, `drt repl` and `drt start` use
natively. The one thing a page cannot share is the loop, which may not
sleep: so a `Session` does not run itself, `tick()` advances it and says
what the page should do next, and the page owns the clock.

Everything the runtime writes -- the C core's `print` (through wasi-libc's
`fd_write`), the REPL's answers, a `drt run:` refusal -- reaches the page
through one sink, in order, as bytes on fd 1 or 2. What a shell shows is
what the page shows, which is why `expected.txt` can be the oracle there.

## The exports (`pkg/drt_web.js`, wasm-bindgen `--target web`)

```
  await init()                            load and start the module (runs wasi-libc's constructors once)
  setPanicHook()                          print a Rust panic's message to the console before it traps
  abiVersion()          -> number         the dv ABI the linked C core speaks
  buildInfo(json)       -> string         `drt buildinfo`, from inside the page

  new DrtTerm(sink)                       sink(fd: 1|2, bytes: Uint8Array), called in order
  term.putFile(path, bytes)               seed a file, making the directories above it
  term.putDir(path)                       an empty directory (a granted scope with no files yet)
  term.getFile(path)    -> Uint8Array | undefined
  term.listFiles()      -> string[]       every file, by absolute path
  term.setCwd(path) / term.cwd()          what relative paths resolve against
  term.exec(argv)       -> DrtSession     argv is a shell's: ["drt", "run", "app.dlua"]
  term.free()

  session.tick()        -> {sleepMs} | {wantsInput: true, continuing} | {done: true, status}
  session.feed(line)    -> boolean        a line for the REPL; false when a blank line was not sent
  session.continuing()  -> boolean        the next line continues an unfinished one (prompt `>> `)
  session.isOver()      -> boolean
  session.free()
```

`tick()` is doc/Wasm.md D6's `Next`, marshalled. The page loop is:

```js
async function drive(session) {
  for (;;) {
    const next = session.tick();
    if (next.sleepMs !== undefined) { await sleep(next.sleepMs); continue; }
    if (next.wantsInput) { session.feed(await readLine(next.continuing)); continue; }
    return next.status;
  }
}
```

Nothing blocks, so the loop runs equally on the main thread and in a
worker. `--help`, `--version`, `buildinfo` and every argument error are
answered inside `exec` and the session is already over on its first
tick, with clap's own conventions: help to fd 1 and status 0, a usage
error to fd 2 and status 2.

**A `DrtTerm` is the process.** Its filesystem is installed as the
module's, and its sink as the module's; make a second one and the first
is no longer wired. One terminal per module, one session at a time.

**Exports do not panic.** On this target a Rust panic is a trap, not an
unwind: `catch_unwind` never runs, `RuntimeError: unreachable` is thrown
into JS, and wasm-bindgen marks the instance terminated, after which every
export throws `Module terminated`. So the bodies return errors instead
(`feed` throws a string; a bad command line is a status), and a page that
catches a trap discards the module. `setPanicHook` makes the reason
visible first.

## The files a page sees

The page seeds a memory filesystem (`drt_platform::fs::MemFs`) and every
path a command opens -- the program, a config, a granted directory -- is a
path the page put there. Paths are absolute or relative to `cwd`. The fs
connector jails exactly as it does over a disk: a config's `scope` names
a directory inside the page's tree, and `..` or an absolute path out of it
is refused in the same words. `putFile` after `exec` is visible to the
running program; what a program writes, `getFile` reads back.

## The terminal: `drt-term.js` and `shell.js`

`drt-term.js` binds a `DrtTerm` to anything with xterm.js's two methods,
`write(text)` and `onData(callback)`:

```js
import init, { DrtTerm } from './drt_web.js';
import { attach } from './drt-term.js';
await init();
const { term } = attach(DrtTerm, xtermTerminal);   // `$ ` appears; type `drt run app.dlua`
term.putFile('/app.dlua', new TextEncoder().encode('print("hello")'));
```

It is the process a shell would be: a `$ ` prompt, a line editor
(printable keys, Backspace, Enter, `^C` drops the line or the running
command, `^D` on an empty line leaves the REPL), the REPL's `dv> ` and
`>> ` prompts when a session wants a line, and output with `\n` made
`\r\n`. `shell.js` behind it is just enough sh for the examples'
`meta.json` commands: `;`, single and double quotes, `$?`, `echo`, and
`drt` -- the real one. Anything else is `command not found`, status 127.
Neither file depends on xterm.js; the suite drives them with a fake
terminal and the page in `browser-test/index.html` with a `<pre>`.

## What the suite proves (`crates/drt-web/browser-test`)

`run.mjs` is `examples/run-all.sh` for a page: every `examples/NN-*/`
with a `meta.json` is seeded into the page under `/examples/NN-*`, its
`cmd` is run through `shell.js` with stdout and stderr merged, its
`normalise` (sed, translated to JavaScript one expression at a time) is
applied to both sides, and the two are diffed. `needs_build: full` and
`needs_network` skip by name and are never counted as passes, as in the
shell version. `buildinfo` inside the page reports `profile: web`, and
that is what decides the skips. Then `repl-script.txt` is typed at
`drt-term.js` and the transcript, echoes removed, is compared with
`repl-expected.txt`, which the native binary produced for the same lines.

On 2026-09-03: 01, 02, 03, 04, 06, 08, 12 and the REPL pass; the rest
skip for needing `full` or a network, exactly as they do under wasmtime.

## Building it

`script/drt-web.sh` (doc/Release.md, "Building for wasm"): `WASI_SDK_PATH`
for the C core and wasi-libc, the wasm-bindgen CLI at the version
`Cargo.toml` pins, and it writes `pkg/` where the page and the suite load
it. Two things the script carries that a reader would otherwise
re-discover:

- **The C core is compiled without `-DLUA_USE_C89`.** diluvium-sys passes
  that flag for `wasm32-unknown-unknown`, from when the target had no
  libc; it makes `lua_Integer` 32 bits, and a millisecond timestamp does
  not fit one. `script/drt-web-cc.sh` removes it until diluvium-sys does
  (doc/Wasm.md §7).
- **The module starts by calling wasi-libc's constructors** (`start` in
  `bindings.rs`), the reactor convention done by hand. Without that call
  wasm-ld wraps every export as a WASI *command* -- constructors before,
  destructors after -- and the destructor flushes stdout into the sink,
  which allocates a JS value through an export, which runs the
  destructors: a stack overflow on the first `print`.

## The swarm exports

The earlier draft's `Swarm` table -- the instances panel's `dvs_*` twins
-- is not in this module yet; the Lab's Instances panel still has
`diluvium_swarm_wasi.wasm` for it. When it lands it lands here, beside
`DrtTerm`, over the same `Deployment` a `drt start` session drives;
doc/Wasm.md M5 is where that is scheduled.
