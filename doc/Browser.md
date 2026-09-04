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
`write(text)` and `onData(callback)`. That is the whole of what a host
writes:

```js
import init, { DrtTerm } from './drt_web.js';
import { attach } from './drt-term.js';

const terminal = new Terminal({ /* your own theme, font, addons */ });
terminal.open(document.getElementById('term'));

await init();
const { term, run, reset } = attach(DrtTerm, terminal, { banner: 'drt' });
term.putFile('/app.dlua', new TextEncoder().encode('print("hello")'));
```

```
  attach(DrtTerm, terminal, { prompt, banner, DrtEditor }) -> handle
  handle.term            the DrtTerm: putFile, putDir, getFile, listFiles, setCwd
  handle.run(line)       submit a line nobody typed; resolves with its exit status
  handle.reset()         abandon whatever is running, return to the prompt
  handle.whenIdle()      resolves at the next moment a line is wanted
  handle.dispose()       unregister the onData handler
```

It is the process a shell would be: a `$ ` prompt, the REPL's `dv> ` and
`>> ` prompts when a session wants a line, and output with `\n` made
`\r\n`. The editing is `DrtEditor`'s -- `ego_cli`'s `Session` over the
same terminal object, so the page has the history, word motions, undo and
guest-completing Tab a tty has, from one implementation rather than a
second that drifts (doc/Wasm.md D8, M8). `attach` takes it as an argument
rather than importing it so a host can pass its own, and `drt-term.js`
decides only *when* a line is wanted and with which prompt. `shell.js` behind it is just enough sh for the examples'
`meta.json` commands: `;`, single and double quotes, `$?`, `echo`, and
`drt` -- the real one. Anything else is `command not found`, status 127.

`run` is there because a panel has buttons as well as a keyboard -- a
"try this example" link, a restored session, a test. It submits the line
through the terminal's own `input`, xterm.js's "as if the user typed
this", so the editor echoes and edits it exactly as it would a person's
keystrokes and what the terminal shows afterwards is what a person doing
it by hand would have seen. It resolves when the command is over, which
`whenIdle` cannot tell you: `whenIdle` answers about now, and a keystroke
the terminal has not delivered yet is not yet running.

Neither file imports xterm.js -- a bundler, an import map and a
`<script>` tag all work, and the object is used duck-typed. The suite
drives them through real xterm.js `Terminal`s in Chromium -- the demo on
`index.html`, the transcript the parity check types at, and
`xterm.html`'s, typed into with real keystrokes -- and every assertion is
read out of a terminal's own rendered buffer. The fake terminals and the
`<pre>` with a keydown handler that used to stand in for two of those are
gone: an editor that moves the cursor and redraws needs a terminal to
interpret it, and a hand-written stand-in would be one more terminal
emulator to get wrong.

## What the suite proves (`crates/drt-web/browser-test`)



`run.mjs` is `examples/run-all.sh` for a page: every `examples/NN-*/`
with a `meta.json` is seeded into the page under `/examples/NN-*`, its
`cmd` is run through `shell.js` with stdout and stderr merged, its
`normalise` (sed, translated to JavaScript one expression at a time) is
applied to both sides, and the two are diffed. `needs_build: full` and
`needs_network` skip by name and are never counted as passes, as in the
shell version, and so does `needs_listener` -- a page has no socket to
bind. `buildinfo` inside the page reports `profile: web`, and that is
what decides the build skips.

Then `repl-script.txt` is typed at `drt-term.js` and the screen it
renders, echoes removed, is compared with `repl-expected.txt`, which the
native binary produced for the same lines. What is compared is the
rendered buffer rather than the bytes written, because since M8 those
bytes carry the editor's cursor moves and redraws and are no longer a
transcript of anything a person saw; runs of horizontal whitespace are
collapsed on both sides, a tab having already become columns on the one
side and not the other. And `xterm.html` is loaded with a
real xterm.js `Terminal`, typed into with real keystrokes (including
Backspace, so the edit path is exercised), asked to `run` a line nobody
typed, and read back from the terminal's own buffer.

On 2026-09-03: 01, 02, 03, 04, 06, 08, 12, the REPL and the xterm
embedding pass; the rest skip for needing `full`, a network, or a port a
page cannot bind, exactly as they do under wasmtime.

## Building it

`script/drt-web.sh` (doc/Release.md, "Building for wasm"): `WASI_SDK_PATH`
for the C core and wasi-libc, the wasm-bindgen CLI at the version
`Cargo.toml` pins, and it writes `pkg/` where the page and the suite load
it. Two things the script carries that a reader would otherwise
re-discover:

- **The C core wants a diluvium new enough to pin its numeric types.**
  Before `44b60fc` upstream, `-DLUA_USE_C89` -- which diluvium-sys passes
  for this target, from when it had no libc -- also made `lua_Integer` 32
  bits, so `host.time()` overflowed in a page and nowhere else.
  `luaconf.h` now pins the types on every target, and this tree's pin is
  past that commit; a build against an older diluvium fails examples 01
  and 12 on the clock (doc/Wasm.md §7).
- **The module starts by calling wasi-libc's constructors** (`start` in
  `bindings.rs`), the reactor convention done by hand. Without that call
  wasm-ld wraps every export as a WASI *command* -- constructors before,
  destructors after -- and the destructor flushes stdout into the sink,
  which allocates a JS value through an export, which runs the
  destructors: a stack overflow on the first `print`.

## The hosts

Three places want an xterm.js terminal with `drt` behind it, and the
surface above is all three of them (doc/Wasm.md M5).

**The diluvium homepage panel.** Its reference implementation is
`diluvium/doc/repl-reference.html`, and this replaces three hand-written
pieces of it: the WASI shim in the page (45 imports and an `ENOSYS`
`Proxy`; `wasi_shim.rs` does it inside the module, so nothing is
imported), the `Repl` class doing arrows, `^A`/`^E`/`^U`, history and Tab
(one editor, doc/Wasm.md D8), and `init_lua`/`repl_eval` as the
evaluation protocol (`repl.dlua` over two queues). The panel gains the
rest of `drt` with it: `drt run`, `drt buildinfo`, capabilities, a
filesystem, and `--help` that matches the binary's.

One thing to settle before that swap: **the homepage REPL is unsealed and
`drt repl` is sealed**. `os`, `io` and `require` are there today and
state persists across lines, because the page is demonstrating the
language; `drt run` seals all of that deliberately (GUARANTEES.md). A
homepage that swapped one for the other without deciding this would
quietly stop being able to demonstrate what it is demonstrating.
doc/Wasm.md M5 and §7 carry the question.

**The Lab.** A Terminal tool in the rail over `drt_web_bg.wasm` in the
kernel worker, plus the Instances panel over the swarm exports below.

**Anyone else.** `drt_web.tar.gz` is a release artifact and the snippet
above is its documentation. A p2p web app embedding a sealed runtime is
not a fourth integration; it is this one.

## The swarm exports

`DrtSwarm`, beside `DrtTerm`, over the same `Deployment` a `drt start`
session drives. The Instances panel's sixteen `dvs_*` calls have twins
here, named the way JavaScript names things and taking ids where `dvs_*`
took pointers -- a page building p2p apps never touches a pointer, so DRT
does not impersonate a C ABI to be adopted. `swarm.js`'s
`swarmCapable(exports)` is where a second backend is recognised, and a
`drtCapable` beside it is the migration.

```js
const sw = new DrtSwarm(0, 0);            // maxInstances, spawnsPerStep; 0 = default
const root = sw.root(bytes);              // caps and budget default to a config's ceiling
while (sw.step() > 0) { /* a round */ }
sw.ids(); sw.parent(id); sw.alive(); sw.slotsAllocated();
sw.caps(id); sw.holds(id, 'host:time'); sw.mayGrant(id, cap); sw.budget(id);
sw.push(id, queue, msgpackBytes); sw.kill(id);
sw.hibernate(id); sw.wake(id); sw.resident(id); sw.wakeOnMessage(id); sw.cachedSize(id);
sw.allowHibernation(b); sw.allowBytecode(b); sw.allowUnsafeStdlib(b); sw.setHostIdentity(s);
sw.free();
```

`new DrtSwarm(max, spawns, configJson)` takes the JSON `drt run --config`
takes, for a page that wants `fs` scoped somewhere; without one it gets
the connectors this build carries that need no scope, which is what a
config-less `drt run` gets. `caps` and `budget` on `root` are a config's
own two fields, so nobody learns a second dialect for them.

Two places the table stops matching, both deliberate. `dvs_last_error`
has no twin: an error is thrown where it happens rather than left for a
host to poll. And `parent` answers `undefined` for an id that is not in
the roster, where `dvs_parent` had only 0 to say for both that and the
root. What a host gains by moving is everything a `Deployment` is over a
bare swarm -- connectors behind the grants, hibernation and wake, the
residency policy -- so the panel stops being a viewer of the C swarm and
becomes a host of this one. +184 KB on the module.

## SSH into the page

`DrtSocket` and `DrtSshServer`, and between them a standard `ssh` client
gets the terminal above. The design and what was measured are
`doc/SshInBrowser.md`; what a host writes is this.

The transport first, because it is useful on its own. `DrtSocket` is a
byte stream whose socket stays in the page: nothing in it imports a
WebSocket API, so a real `WebSocket`, an `RTCDataChannel` or a relayed
pair all work, and the page's whole side is three calls.

```js
const server = new DrtSshServer(hostKey, authorizedKeys);
const socket = server.serve((shell) => {
  const terminal = terminalFor(shell);              // ssh-terminal.js
  attach(DrtTerm, terminal, { DrtEditor });         // the same shell as a tab
});

ws.binaryType = 'arraybuffer';
ws.onmessage = (e) => socket.deliver(new Uint8Array(e.data));
ws.onclose = () => socket.close();
for (;;) {                                          // drt -> the wire
  const out = await socket.nextOutgoing();
  if (out === undefined) break;
  ws.send(out);
}
```

`hostKey` is an OpenSSH private key the page **keeps**;
`DrtSshServer.generateHostKey()` hands one back rather than holding it,
because a host key that changes on reload trains whoever connects to click
through the warning that says it changed. `server.fingerprint` is the
`SHA256:...` string `ssh` prints, so a page can show it and be checked
against instead of trusted on first use.

`authorizedKeys` is the contents of an `authorized_keys` file. There is no
"accept any key": the empty string authenticates nobody, and
`server.authorized` counts what got in, so a host can say `0` out loud.
There is no password method either. Both are the ssh *client* connector's
posture pointed the other way (`GUARANTEES.md`).

`ssh-terminal.js` is the adapter, and it is small on purpose: an SSH
session already has bytes in, bytes out and a window, which is what
`attach` takes. The editor behind it is `ego_cli`'s, the same one a tab
gets -- there is no second terminal implementation. A client that resizes
its window is picked up on the next keystroke, because that is when
`ego_cli` asks a terminal how big it is.

`socket.startEcho()` is the transport without a protocol on it: bytes in,
upper-cased bytes back. It ships so a host can check its plumbing before
SSH is in the way.

### Being reachable: `relay-leg.js`

The snippet above assumes the page already has a socket. A page has no
inbound address, so where the socket comes from is the other half of the
question, and the answer is DRT's rendezvous relay -- the same one a
laptop behind CGNAT uses. The page parks an outbound leg by label; a
caller claims the label through `drt tunnel`; the relay splices them.

```js
const leg = park(
  `wss://${label}--tunnel.${zone}/park/${label}?k=${parkKey}`,
  () => server.serve(onShell),                     // one socket per claim
  { onEvent: (name) => show(name) },               // parked | claimed | closed
);
```

Then, from anywhere:

```sh
ssh -o ProxyCommand="drt tunnel wss://<label>--tunnel.<zone>/s/<label>?k=<caller>" you@<label>
```

`open` is called once per claim, and a claimed leg is replaced
immediately -- the relay's replenish-on-claim, which is why a second
caller finds somebody home. `onEvent` is where a host gets presence
without asking anything: a panel that says *the tab is reachable* is
watching `parked` and `closed`. `leg.close()` stops it, and a label with
no parked leg tells a caller nobody is home.

Nothing in the relay's protocol is new here: it is URLs, HTTP status and
binary frames, built so `websocat` could speak it, and a browser
`WebSocket` is that kind of client. `relay-leg.js` is a socket, a
first-byte test and a re-park.

+1,521,413 bytes on the module, which is what a server, two key exchanges
and a cipher suite cost.
