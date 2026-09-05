# Changelog

All notable changes to DRT are recorded here.

Generated from `CHANGELOG.yaml`, which is the source of truth --
edit that file, then run `script/changelog.py generate`.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

DRT versions independently of diluvium and *records* the coupling
rather than encoding it: each entry names the dv ABI it speaks and
the diluvium revision it embeds, the same facts `BUILDINFO.txt`
carries in the release. See `doc/Release.md`.

## [0.5.0rc2] - 2026-09-04 (prerelease)

`v0.5.0rc2` &middot; dv ABI 1 &middot; diluvium `f4d52516380c`

v0.5.0rc1 with `rest` fixed, cut the day the fix landed because
the bug it carries has no workaround: `rest` is how a guest
reaches a model, and a deployment that cannot complete one call
cannot classify, draft, summarize or repair anything. Vera is
deployed, ingesting mail, and was blocked entirely behind it.

**One change from rc1**, under Fixed below. `rest` read a response
body to EOF rather than to the framing its head declares, so every
server that closes without a TLS `close_notify` -- Amazon Bedrock
among them -- failed a call it had already answered in full. The
notes are otherwise rc1's, because the rest of the code is.

**Still a candidate.** The wasm surface is what v0.5.0 is for and
it is still what no consumer has built against; that test is the
one it has not had. Not mirrored, and not `latest`: `install.sh`
keeps resolving to the newest stable release, which is v0.4.2.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `ssmtp`, `exec`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`
- `wasi`: `time`, `fs`, `crypto`, `sql`, `listen`
- `web`: `time`, `fs`, `crypto`

### Added

- **`drt` on `wasm32-wasip2`, released as `drt_wasip2.wasm`.** The
  binary itself, built with the `wasi` profile -- `time`, `fs`,
  `crypto`, `sql`, `listen` -- and run under wasmtime with
  `script/drt-wasip2.sh`, which carries the flags the C core's
  `setjmp`/`longjmp` lowering needs. It proves itself the way the
  native legs do and then harder: the examples gate runs through
  the wrapper before the artifact is uploaded.
- **`drt` in a browser, released as `drt_web.tar.gz`.** The `web`
  profile with the C core and wasi-libc linked into one module, so
  it imports nothing but wasm-bindgen's glue -- wasi-libc's
  seventeen syscalls are defined inside it, and `fd_write` reaches
  the same sink the runtime's own output does. The tarball carries
  the module, its glue, and `drt-term.js`/`shell.js`: a host
  constructs its own xterm.js `Terminal`, calls `attach`, and has a
  `$ ` prompt running `drt run`, `drt repl` and `drt buildinfo`.
  The gate is the examples in Chromium, a REPL transcript diffed
  against the native binary's, and a real xterm.js typed into.
- **`drt start` serves from wasmtime.** wasip2 has sockets and no
  threads, so the listener gained a second acceptor: non-blocking
  `std::net`, one state machine per connection, stepped from the
  drive loop. The bridge's contract is unchanged -- one request per
  connection as a message on a queue, headers by allowlist only --
  and `examples/17-serving-http` is the deployment, curl'd both
  natively and under wasmtime.
- **Line editing in `drt repl`, behind the `cli` feature.** History,
  word motions, undo, and Tab whose candidates are the names the
  running instance answers with rather than a list the host
  hard-coded. One editor (`ego-cli`) rather than one per host --
  the same `Session` and the same completer on a tty and in a
  page, where `attach` drives the host's own xterm.js object.
  In `slim` as well as `full`, costing +150 KB and no async
  runtime, because the backend under it blocks on the terminal
  instead of driving an executor; +129 KB in the browser. `drt
  repl` reading a pipe is unedited as before, prompts on stderr,
  and so is `drt repl` under wasmtime, which has no way to ask
  for raw mode.
- **`drt repl --unsafe`, and the swarm exports.** Two things a
  browser host asked for. The flag evaluates with `os`, `io` and
  `require` in scope, which a *language* REPL needs and a sealed
  guest is not given: it lifts the stdlib seal and nothing else,
  so the capability grants and the budget still decide what a
  hostcall reaches, and the banner names what is off rather than
  looking like a sealed REPL. It costs replayability and makes the
  budget approximate. And `DrtSwarm` puts the instances table --
  root, step, the roster, the capability questions, hibernate and
  wake -- beside `DrtTerm`, over the deployment `drt start`
  drives, so a panel driving the C swarm's sixteen entry points
  has twins for them here.
- **`crates/drt-platform`.** The four places DRT touches a platform
  -- clock, entropy, the filesystem, stdio -- behind one seam, so
  nothing above them is target-aware. In a page the clock is
  `web-time`, the filesystem is a `MemFs` the page seeds, and stdio
  is a sink the page installs.

### Changed

- **wasm-bindgen `=0.2.114` -> `=0.2.127`, and a workaround
  deleted.** 0.2.114 refused a module carrying the C core's
  `try_table` unless it exported `__instance_terminated`, so
  `drt-web` defined that global itself; 0.2.127 defines it, and the
  definition is gone. The pin could not move earlier because the
  ego crates pinned 0.2.114 and a workspace holds one version --
  they moved, each tagging `pre-wasm-bindgen-0.2.127` at the commit
  before, and DRT followed. Nothing hard-codes the version twice:
  `script/drt-web.sh` and the `browser` job read the CLI's version
  off the crate's pin, so this was one line and the rest followed.
- **`sql` is on `rusqlite 0.40`.** A version bump and no source
  change: the connector uses `Connection`, `OpenFlags`, `ToSql`
  and `types::{Value, ValueRef}`, none of which moved. It matters
  because `libsqlite3-sys` carries a `links` key, so a build can
  hold one SQLite and not two -- which is what stood between DRT
  and an aloelite volume as an `fs` backend (doc/Aloelite.md §4).
- **The drive loop inverted, and the hostcall pump defers.**
  `run`, `repl` and `start` no longer own loops that block:
  `tick()` advances an instance as far as it can and returns what
  the host should do next, and the host owns the sleeping. A
  connector that cannot answer at once is parked in an in-flight
  table and polled on the loop's cadence instead of stalling every
  instance -- which also removes the failure mode
  `doc/Failure-Modes.md` records for `rest`.
- The command surface moved from `main.rs` into `drt::cli`, so a
  page parses the same command line the binary parses, with the
  same `--help` and the same exit statuses.

### Removed

- **The browser tier's JS host bridge.** `drt-web` was an `Engine`
  over a JS-hosted interpreter, which was SPEC.md §4's fallback for
  the case where the C core could not be linked into the same
  module. It can, so the fallback is gone rather than kept as a
  second engine path.

### Fixed

- **`rest` read past the body it already had, and failed the call
  for how the peer hung up.** A server that closes the TCP
  connection without a TLS `close_notify` -- Amazon Bedrock, and a
  great many others -- made every `rest/get` and `rest/post` against
  it fail with `read: peer closed connection without sending TLS
  close_notify`, with every byte of the body already in hand. The
  read loop ended on EOF rather than on the framing the response
  head declares, so the read that failed was the one *after* the
  last one that mattered: a deployment could not complete one model
  call while `curl` answered 200 to the same request, the same
  token and the same body, seconds apart.

  The loop now stops where `content-length` or the terminating
  chunk says the body ends, and a close without `close_notify` is
  judged rather than raised -- it ends a body the framing says is
  complete, and fails one it says is short, naming what was expected
  and what arrived (`read 5 of 11 bytes, then the peer closed
  without close_notify`) because the error it replaces named neither
  and reads as a broken network. A response framed by the close
  alone is unchanged: the close is its only ending, and telling that
  from a truncation is exactly what `content-length` is for.

  Two things fall out of reading the framing rather than the
  connection. A `content-length` beyond the response-header bound
  still frames the body, where before a long-headed response fell
  back to waiting for EOF; and a server that ignores `connection:
  close` no longer holds the call open until `timeout_ms`.
  Reported as issue #10, which was misdiagnosed twice before it
  landed there -- a TLS-layer error reads as "the connection is
  broken", which sends you to the network and the credentials
  rather than to the client's own read loop.
- The fs connector's jail tests for a root with `has_root` rather
  than `is_absolute`, which `std` answers differently on
  `wasm32-unknown-unknown` -- an absolute path was refused
  natively and by a different rule in a page.

### Known issues

- **A request arriving before the program declares its queue is
  answered 503 rather than held.** The window is the gap between
  the bind and the program's `queue.declare` — tens of milliseconds
  for a program that declares first, the whole of its boot for one
  that does config or schema work first. It is silent, and it lands
  on a caller that handshakes once at startup and never asks again.
  Until the fix, declare every listener queue in the program's first
  statements. Fixed after this candidate by `admit_timeout_ms` on
  the listener (issue #11).


## [0.5.0rc1] - 2026-09-03 (prerelease)

`v0.5.0rc1` &middot; dv ABI 1 &middot; diluvium `f4d52516380c`

The wasm port, as a candidate: the same code v0.5.0 will carry,
tagged so the work waiting on it can start against a tag rather
than a branch name. The Lab's Terminal and Instances panels and
the diluvium homepage's REPL are the three consumers, and what
they need is `drt_web.tar.gz` and the exports below.

**A candidate, not the release.** Nothing here is known to be
wrong -- the examples gate passes on all three targets and the
browser suite is ten checks -- but no consumer has yet built
against the surface, and the first one to try is the test the
surface has not had. v0.5.0 follows when it has.

Not mirrored, and not `latest`: an `install.sh` still resolves
to the newest stable release, which is v0.4.2.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `ssmtp`, `exec`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`
- `wasi`: `time`, `fs`, `crypto`, `sql`, `listen`
- `web`: `time`, `fs`, `crypto`

### Added

- **`drt` on `wasm32-wasip2`, released as `drt_wasip2.wasm`.** The
  binary itself, built with the `wasi` profile -- `time`, `fs`,
  `crypto`, `sql`, `listen` -- and run under wasmtime with
  `script/drt-wasip2.sh`, which carries the flags the C core's
  `setjmp`/`longjmp` lowering needs. It proves itself the way the
  native legs do and then harder: the examples gate runs through
  the wrapper before the artifact is uploaded.
- **`drt` in a browser, released as `drt_web.tar.gz`.** The `web`
  profile with the C core and wasi-libc linked into one module, so
  it imports nothing but wasm-bindgen's glue -- wasi-libc's
  seventeen syscalls are defined inside it, and `fd_write` reaches
  the same sink the runtime's own output does. The tarball carries
  the module, its glue, and `drt-term.js`/`shell.js`: a host
  constructs its own xterm.js `Terminal`, calls `attach`, and has a
  `$ ` prompt running `drt run`, `drt repl` and `drt buildinfo`.
  The gate is the examples in Chromium, a REPL transcript diffed
  against the native binary's, and a real xterm.js typed into.
- **`drt start` serves from wasmtime.** wasip2 has sockets and no
  threads, so the listener gained a second acceptor: non-blocking
  `std::net`, one state machine per connection, stepped from the
  drive loop. The bridge's contract is unchanged -- one request per
  connection as a message on a queue, headers by allowlist only --
  and `examples/17-serving-http` is the deployment, curl'd both
  natively and under wasmtime.
- **Line editing in `drt repl`, behind the `cli` feature.** History,
  word motions, undo, and Tab whose candidates are the names the
  running instance answers with rather than a list the host
  hard-coded. One editor (`ego-cli`) rather than one per host --
  the same `Session` and the same completer on a tty and in a
  page, where `attach` drives the host's own xterm.js object.
  In `slim` as well as `full`, costing +150 KB and no async
  runtime, because the backend under it blocks on the terminal
  instead of driving an executor; +129 KB in the browser. `drt
  repl` reading a pipe is unedited as before, prompts on stderr,
  and so is `drt repl` under wasmtime, which has no way to ask
  for raw mode.
- **`drt repl --unsafe`, and the swarm exports.** Two things a
  browser host asked for. The flag evaluates with `os`, `io` and
  `require` in scope, which a *language* REPL needs and a sealed
  guest is not given: it lifts the stdlib seal and nothing else,
  so the capability grants and the budget still decide what a
  hostcall reaches, and the banner names what is off rather than
  looking like a sealed REPL. It costs replayability and makes the
  budget approximate. And `DrtSwarm` puts the instances table --
  root, step, the roster, the capability questions, hibernate and
  wake -- beside `DrtTerm`, over the deployment `drt start`
  drives, so a panel driving the C swarm's sixteen entry points
  has twins for them here.
- **`crates/drt-platform`.** The four places DRT touches a platform
  -- clock, entropy, the filesystem, stdio -- behind one seam, so
  nothing above them is target-aware. In a page the clock is
  `web-time`, the filesystem is a `MemFs` the page seeds, and stdio
  is a sink the page installs.

### Changed

- **The drive loop inverted, and the hostcall pump defers.**
  `run`, `repl` and `start` no longer own loops that block:
  `tick()` advances an instance as far as it can and returns what
  the host should do next, and the host owns the sleeping. A
  connector that cannot answer at once is parked in an in-flight
  table and polled on the loop's cadence instead of stalling every
  instance -- which also removes the failure mode
  `doc/Failure-Modes.md` records for `rest`.
- The command surface moved from `main.rs` into `drt::cli`, so a
  page parses the same command line the binary parses, with the
  same `--help` and the same exit statuses.

### Removed

- **The browser tier's JS host bridge.** `drt-web` was an `Engine`
  over a JS-hosted interpreter, which was SPEC.md §4's fallback for
  the case where the C core could not be linked into the same
  module. It can, so the fallback is gone rather than kept as a
  second engine path.

### Fixed

- The fs connector's jail tests for a root with `has_root` rather
  than `is_absolute`, which `std` answers differently on
  `wasm32-unknown-unknown` -- an absolute path was refused
  natively and by a different rule in a page.

### Known issues

- **`rest` fails against a server that closes without a TLS
  `close_notify`.** Every `rest/get` and `rest/post` to such a
  server -- Amazon Bedrock among them -- comes back as `read: peer
  closed connection without sending TLS close_notify`, with the
  whole body already received. There is no workaround from a guest:
  `rest` is how a guest reaches a model, so a deployment on this
  candidate cannot complete one model call. Fixed in v0.5.0rc2
  (issue #10).


## [0.5.0] - unreleased

`v0.5.0` &middot; dv ABI 1 &middot; diluvium `f4d52516380c`

Two wasm artifacts, and one runtime behind all three of them.
`drt_wasip2.wasm` is `drt` itself under wasmtime; `drt_web.tar.gz`
is the same `drt` in a page, with the C core linked into the module
and a terminal contract to attach xterm.js to. Neither is a second
implementation: the examples gate is the conformance oracle on
every target, diffing one `expected.txt` natively, under wasmtime,
and in Chromium.

A minor digit rather than a patch, by the rule the connector list
makes: two profiles are new (`wasi`, `web`) and `wasi` carries
`listen`, so BUILDINFO's list is not v0.4.2's and a package
declaring `requires.connectors` resolves differently against them.
`full` and `slim` are unchanged, and nothing that existed behaves
differently on a native build.

The plan and its measurements are `doc/Wasm.md`; the embedding
contract is `doc/Browser.md`; `doc/Platforms.md` is the matrix.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `ssmtp`, `exec`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`
- `wasi`: `time`, `fs`, `crypto`, `sql`, `listen`
- `web`: `time`, `fs`, `crypto`

### Added

- **`drt` on `wasm32-wasip2`, released as `drt_wasip2.wasm`.** The
  binary itself, built with the `wasi` profile -- `time`, `fs`,
  `crypto`, `sql`, `listen` -- and run under wasmtime with
  `script/drt-wasip2.sh`, which carries the flags the C core's
  `setjmp`/`longjmp` lowering needs. It proves itself the way the
  native legs do and then harder: the examples gate runs through
  the wrapper before the artifact is uploaded.
- **`drt` in a browser, released as `drt_web.tar.gz`.** The `web`
  profile with the C core and wasi-libc linked into one module, so
  it imports nothing but wasm-bindgen's glue -- wasi-libc's
  seventeen syscalls are defined inside it, and `fd_write` reaches
  the same sink the runtime's own output does. The tarball carries
  the module, its glue, and `drt-term.js`/`shell.js`: a host
  constructs its own xterm.js `Terminal`, calls `attach`, and has a
  `$ ` prompt running `drt run`, `drt repl` and `drt buildinfo`.
  The gate is the examples in Chromium, a REPL transcript diffed
  against the native binary's, and a real xterm.js typed into.
- **`drt start` serves from wasmtime.** wasip2 has sockets and no
  threads, so the listener gained a second acceptor: non-blocking
  `std::net`, one state machine per connection, stepped from the
  drive loop. The bridge's contract is unchanged -- one request per
  connection as a message on a queue, headers by allowlist only --
  and `examples/17-serving-http` is the deployment, curl'd both
  natively and under wasmtime.
- **Line editing in `drt repl`, behind the `cli` feature.** History,
  word motions, undo, and Tab whose candidates are the names the
  running instance answers with rather than a list the host
  hard-coded. One editor (`ego-cli`) rather than one per host --
  the same `Session` and the same completer on a tty and in a
  page, where `attach` drives the host's own xterm.js object.
  In `slim` as well as `full`, costing +150 KB and no async
  runtime, because the backend under it blocks on the terminal
  instead of driving an executor; +129 KB in the browser. `drt
  repl` reading a pipe is unedited as before, prompts on stderr,
  and so is `drt repl` under wasmtime, which has no way to ask
  for raw mode.
- **`drt repl --unsafe`, and the swarm exports.** Two things a
  browser host asked for. The flag evaluates with `os`, `io` and
  `require` in scope, which a *language* REPL needs and a sealed
  guest is not given: it lifts the stdlib seal and nothing else,
  so the capability grants and the budget still decide what a
  hostcall reaches, and the banner names what is off rather than
  looking like a sealed REPL. It costs replayability and makes the
  budget approximate. And `DrtSwarm` puts the instances table --
  root, step, the roster, the capability questions, hibernate and
  wake -- beside `DrtTerm`, over the deployment `drt start`
  drives, so a panel driving the C swarm's sixteen entry points
  has twins for them here.
- **`crates/drt-platform`.** The four places DRT touches a platform
  -- clock, entropy, the filesystem, stdio -- behind one seam, so
  nothing above them is target-aware. In a page the clock is
  `web-time`, the filesystem is a `MemFs` the page seeds, and stdio
  is a sink the page installs.

### Changed

- **wasm-bindgen `=0.2.114` -> `=0.2.127`, and a workaround
  deleted.** 0.2.114 refused a module carrying the C core's
  `try_table` unless it exported `__instance_terminated`, so
  `drt-web` defined that global itself; 0.2.127 defines it, and the
  definition is gone. The pin could not move earlier because the
  ego crates pinned 0.2.114 and a workspace holds one version --
  they moved, each tagging `pre-wasm-bindgen-0.2.127` at the commit
  before, and DRT followed. Nothing hard-codes the version twice:
  `script/drt-web.sh` and the `browser` job read the CLI's version
  off the crate's pin, so this was one line and the rest followed.
- **`sql` is on `rusqlite 0.40`.** A version bump and no source
  change: the connector uses `Connection`, `OpenFlags`, `ToSql`
  and `types::{Value, ValueRef}`, none of which moved. It matters
  because `libsqlite3-sys` carries a `links` key, so a build can
  hold one SQLite and not two -- which is what stood between DRT
  and an aloelite volume as an `fs` backend (doc/Aloelite.md §4).
- **The drive loop inverted, and the hostcall pump defers.**
  `run`, `repl` and `start` no longer own loops that block:
  `tick()` advances an instance as far as it can and returns what
  the host should do next, and the host owns the sleeping. A
  connector that cannot answer at once is parked in an in-flight
  table and polled on the loop's cadence instead of stalling every
  instance -- which also removes the failure mode
  `doc/Failure-Modes.md` records for `rest`.
- The command surface moved from `main.rs` into `drt::cli`, so a
  page parses the same command line the binary parses, with the
  same `--help` and the same exit statuses.

### Removed

- **The browser tier's JS host bridge.** `drt-web` was an `Engine`
  over a JS-hosted interpreter, which was SPEC.md §4's fallback for
  the case where the C core could not be linked into the same
  module. It can, so the fallback is gone rather than kept as a
  second engine path.

### Fixed

- **A request arriving before the program declared its queue was
  refused rather than held.** A listener accepts from the moment
  the process binds it, which is before the program has run a line,
  so a request naming a queue the program has not declared yet was
  answered `503 the program declares no request queue` on the spot
  — a definitive answer to a question the deployment had not
  finished hearing. Stepping the root before delivering covered a
  program that declares before its first park and nothing else: one
  that reads config or migrates a schema first parks with the queue
  still absent.

  The wait is now `admit_timeout_ms` on the listener, defaulting to
  two seconds: a request waits for its queue to exist, and past the
  grace the old refusal is still what it gets, because a program
  that has declared nothing by then declares nothing. `0` restores
  the immediate refusal exactly. The connection's own
  `conn_deadline_ms` still bounds everything — a held request whose
  caller has already given up is dropped rather than answered — so
  this adds a wait, never a hang.

  The window is tens of milliseconds and it lands on precisely the
  caller that cannot survive it: one that handshakes **once** at
  startup. Reported (issue #11) against prosody's `mod_rest`, which
  probes its component once and settles on JSON or XML for the life
  of the process, so a 503 in that window left two healthy services
  that never exchanged a message, with nothing in either log to say
  why. The failure is silent by construction, which is why this is
  a fix and not a documented sharp edge.
- **`rest` read past the body it already had, and failed the call
  for how the peer hung up.** A server that closes the TCP
  connection without a TLS `close_notify` -- Amazon Bedrock, and a
  great many others -- made every `rest/get` and `rest/post` against
  it fail with `read: peer closed connection without sending TLS
  close_notify`, with every byte of the body already in hand. The
  read loop ended on EOF rather than on the framing the response
  head declares, so the read that failed was the one *after* the
  last one that mattered: a deployment could not complete one model
  call while `curl` answered 200 to the same request, the same
  token and the same body, seconds apart.

  The loop now stops where `content-length` or the terminating
  chunk says the body ends, and a close without `close_notify` is
  judged rather than raised -- it ends a body the framing says is
  complete, and fails one it says is short, naming what was expected
  and what arrived (`read 5 of 11 bytes, then the peer closed
  without close_notify`) because the error it replaces named neither
  and reads as a broken network. A response framed by the close
  alone is unchanged: the close is its only ending, and telling that
  from a truncation is exactly what `content-length` is for.

  Two things fall out of reading the framing rather than the
  connection. A `content-length` beyond the response-header bound
  still frames the body, where before a long-headed response fell
  back to waiting for EOF; and a server that ignores `connection:
  close` no longer holds the call open until `timeout_ms`.
  Reported as issue #10, which was misdiagnosed twice before it
  landed there -- a TLS-layer error reads as "the connection is
  broken", which sends you to the network and the credentials
  rather than to the client's own read loop.
- The fs connector's jail tests for a root with `has_root` rather
  than `is_absolute`, which `std` answers differently on
  `wasm32-unknown-unknown` -- an absolute path was refused
  natively and by a different rule in a page.


## [0.4.2] - 2026-09-03

`v0.4.2` &middot; dv ABI 1 &middot; diluvium `515160f64587`

Local `exec`: the one hostcall family the C host answered and DRT
did not, and the one the instruction budget cannot reach. The
contract is `dhost_exec.c`'s to the sentence, a `.host.lua` saying
`exec = true` loads unchanged, and the scope gains the thing the C
host had nowhere to put: the programs a call may start.

A patch digit for a new connector, by the owner's decision
(`doc/Release.md`): nothing that existed changed, and the check a
package makes is by name against BUILDINFO's connector list, never
against the version. That list is not v0.4.1's -- `full` gains
`exec`, `slim` is unchanged -- so a package declaring
`requires.connectors` with `exec` in it is admissible against the
`full` artifact and refused by name against `slim`.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `ssmtp`, `exec`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **`exec/run`, the honest escape hatch.** `{argv, stdin?,
  timeout_ms?, cwd?}` answers `{status, stdout, stderr}`, exactly as
  `host/dhost_exec.c` answers it, so a program written against
  `diluvium-host` runs here unchanged. `argv` is a vector handed to
  exec and never a shell string, so there is nothing a quote can
  escape from; a nonzero exit is an answer, read the way a script
  reads `$?`, and a program that does not exist is `127`. `error` is
  the call's own failure only: the deadline, a cap, a malformed
  request, each in the C host's words.

  Three bounds, all the deployment's, because the instruction
  budget cannot reach a subprocess. `max_timeout_ms` is a ceiling a
  call may ask below and never above; at the deadline the child is
  killed with SIGKILL, and the kill sweeps its whole process group
  on every exit path, so nothing `exec/run` starts outlives the
  call. `max_output_bytes` caps each stream and stdin, refusing
  rather than truncating past it. Both default to the C host's
  numbers.

  And one the C host could not take: `allow`, a list of programs by
  absolute path. A call naming anything else is refused by name and
  never started; an entry that is not there is a refusal at
  startup, by name, like every other unreachable scope. Compared
  after symlinks are resolved on both sides, so `/bin/sh` matches on
  a box where `/bin` is `/usr/bin`. Leave it out and the behaviour
  is the C host's: whatever `PATH` finds.

  `full` only, and not for a dependency -- it links nothing. Off
  until a config names `connectors.exec`, and announced on stderr
  when one does: granting it is leaving the sandbox, and
  GUARANTEES.md's "loud flag" is now a sentence the process prints
  rather than a promise. `examples/16-exec` is the app; `10-ssh-exec`
  no longer claims there is no local exec, and its third run, which
  showed the config being refused, is gone because the config is no
  longer refused.

  Honest about one thing the C host is honest about too: the
  connector answers synchronously, so a running child stalls every
  guest in the deployment until it exits or hits its deadline. Bound
  it tight. The deferred pump planned in `doc/Wasm.md` lifts that
  for every connector at once.

### Changed

- **The connector set grows.** `full` carries `exec`; `slim` is
  unchanged. `drt buildinfo` reports it, so a package's
  `requires.connectors` can name it.


## [0.4.1] - 2026-09-03

`v0.4.1` &middot; dv ABI 1 &middot; diluvium `515160f64587`

`ssmtp/send` threads. A reply can name the message it answers, so a
client files it under that conversation rather than beside it. And
the examples gate passes under its own documented command, which
it did not.

A patch, honestly: the connector set is v0.4.0's, so a package
declaring `requires.connectors` sees nothing new. The call surface
grew by two optional fields and nothing was removed.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `ssmtp`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **`ssmtp/send` takes `in_reply_to` and `references`.** A reply had
  no way to say what it answered: the call was `{to, subject, body}`,
  so the only threading a program could do was `Re:` on the subject,
  which is a guess a client may or may not make. `in_reply_to` is
  the parent's `Message-ID` as its header spelled it; `references`
  is the thread, oldest first, and defaults to `in_reply_to` --
  which is what a reply to a thread's first message carries.

  These two are the guest's to set because neither routes: a relay
  reads nothing from them and a client only threads on them. So the
  check is on shape rather than on trust. An id reads `<id@host>`;
  the brackets are supplied when a source such as JMAP strips them,
  and a token that cannot be an id is refused by name and by field.
  A line ending inside either is a **fold**, not an injection: a
  `References` line long enough to have been folded by its sender
  arrives with a CRLF in it, so whitespace separates ids there rather
  than refusing the value, and nothing of the guest's text reaches
  the wire as written -- what follows a fold is one more id inside
  the same header, never a header of its own. Long lists fold at 78
  on the way out, per RFC 5322.

  No other header is the guest's, still. What a message could also
  carry, and the argument for each, is in `doc/Next.md` under
  `ssmtp`: a `Message-ID` the connector mints and returns, `Date`,
  `Auto-Submitted`, `Cc` under the same allowlist -- and `Reply-To`,
  which is the scope's and never the program's.

  `examples/15-sending-mail` sends a third message, a reply, so the
  two lines are on the transcript where a client looks for them.

### Fixed

- **`examples/run-all.sh` broke `13` and `15` under its own documented
  command.** `DRT=../target/release/drt ./run-all.sh` is relative to
  `examples/`, and the runner resolved that to an absolute path for
  `PATH` but left the relative one in the environment -- so the two
  `demo.sh` scripts that honour `$DRT`, running from inside their own
  directories, named a file that was not there. `13` failed by name
  and `15` waited for a relay connection that never came until the
  timeout. The absolute path is exported now, and the fourteen that
  only use `PATH` are unchanged.


## [0.4.0] - 2026-09-02

`v0.4.0` &middot; dv ABI 1 &middot; diluvium `515160f64587`

The capability model made true where it was not, a NAT diagnostic
that finishes, outbound mail, and four ways of being confidently
wrong that this release stopped being.

**The guarantees first.** Budgets did not attenuate at spawn, `sql`
discarded an open transaction in silence, and the no-reactor panic
that shipped an uncallable connector in v0.3.1 had no test that
could fail. Those are promises DRT already made and did not keep,
and two of the three were found twice independently -- once by this
repository's own examples pass, once by discofetch reading the code.
That is the argument for doing them before any feature, and it is
why they are at the top of this list.

**`netcheck` grew the half it was missing and lost four wrong
answers.** It can now ask a reflect edge what it saw, pin a source
port so two vantages are a measurement rather than two numbers, and
probe a port from an edge it has not contacted -- which makes
`direct` reachable for the first time. Along the way it stopped
giving `v6-direct` to networks it had measured nothing about,
stopped calling two ephemeral ports a comparison, stopped calling
two views of one destination an endpoint comparison, and started
saying *why* a measurement is missing. Every one of those was found
by running it on a real network rather than by reading it.

**`ssmtp`**, `rest`'s sibling: a recipient allowlist, and the relay
credential and envelope sender held by the deployment so a program
sends mail without the password and cannot forge its From line.

**The diluvium pin moves to 5.5.1_build12p1**, which v0.4.0rc1
deliberately deferred. It closes FM-2 and the instruction budget
switching itself off; the pcall *loop* remains, and FM-4 says so.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `ssmtp`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **`Connector::finish`**, and `Dispatcher::finish` over it. A
  connector holding state that outlives a hostcall says at teardown
  whether that state ended well; each string it returns is one thing
  that did not. Most connectors hold nothing and take the default.

  It exists for `sql` and is written so the next one does not need a
  new seam.
- **`examples/15-sending-mail`.** The `ssmtp` connector as a
  run-through, and the one example whose point is best made by the
  wire rather than the output: the body's lone `.` arrives as `..`,
  and a subject asking to be `From: me@evil.example` lands *below*
  the deployment's real `From:` line. Ships a fake relay so it runs
  with no account and no network.
- **`examples/run-all.sh` skips what a build cannot run, instead of
  failing it.** `cargo build` with no flags is a **slim** binary, and
  eight examples need connectors or verbs slim does not carry -- so
  the obvious invocation failed eight examples at once, each with a
  diff whose real content was "this build does not carry that".

  `meta.json` gains `needs_build`, the runner reads the profile from
  `drt buildinfo`, and a mismatch is a named skip that is never
  counted as a pass -- the same rule `needs_network` already had. It
  also says the command to get the whole set, up front, rather than
  after eight diffs.
- **`drt netcheck --reflect <url>`, repeatable, and a `netcheck`
  cargo feature to carry it.** An edge is asked what it saw over TCP;
  the answer fills the `address` line and one vantage of `tcp map`,
  keyed by the `edge` the edge names itself.

  Its own feature rather than riding `stun`, because asking an edge
  means an HTTP client and a TLS stack and the STUN *server* has no
  business carrying either — the server is what a small deployment
  runs. `full` only, and it adds no new crate: the TLS stack is the
  one `rest` already links. Priced and accepted: a `--features stun`
  build keeps the server and loses the verb.

  **The JSON form, not `?format=addr-port`.** `REFLECT-NAT.md` §5 says
  to key these by the `edge` field and `addr-port` carries no edge —
  it is `ADDRESS PORT` and nothing else. One JSON fetch gives address,
  port and edge together, so it is one request rather than two, and
  two requests would be two connections reporting two source ports.
- **`drt netcheck --port <n>`, experimental, and `direct` is
  reachable at last.** The verdict that requires an inbound connect
  had no flag in v0.4.0 that could produce one. `--port` asks a probe
  edge to connect back to the address it observes and reports
  `connected` / `refused` / `timeout`, against the contract
  `deploy/probe/test-probe.sh` already tests. Repeatable, sequential,
  bounded at 8 — the prober rate-limits per observed address and a
  diagnostic that walks a range is what that limit is for.

  **The client obligation is enforced, not documented.**
  `NETCHECK-SPEC.md` §3: with a prober on both gates, the asymmetry
  that made the probe safe becomes the client's job, because a SYN
  from an address the caller just contacted can traverse the mapping
  the caller's own request created and answer `connected` when
  nothing out there can reach them. So `--probe-at` names a vantage,
  and a probe from an edge this run already used for reflect is
  **refused by name** rather than trusted. `connected` is the only
  result that reaches `direct`, whose advice is "forward the port",
  so a false one is the most expensive answer this tool can give.

  A 429 is silence: rate-limited reads `not measured`, never
  `refused`. And the token is a slot, carried and never parsed — no
  minting exists, and when it does it is one more query parameter
  with the response shape unchanged, so a parser for it would only
  couple DRT to a format discofetch may rotate.

  Experimental because the server half is not deployed: the prober is
  not on gate2 yet.
- **`rest` sent no `User-Agent`, which Cloudflare blocks.**
  `api.discofetch.net` is behind Cloudflare, whose rule 1010 refuses
  unidentified client signatures — so a guest calling that API got an
  error page rather than an API, from a connector that had said
  nothing about itself. It now sends `drt/<version>` when nobody
  named one, and a guest or an operator naming their own still wins:
  this only fills the silence.
- **`drt netcheck --reflect-at <address>`**, repeatable: ask
  `--reflect` at an address rather than at the one its name resolves
  to, keeping the name in `Host`. `curl --resolve` by another name.

  It exists because the design is one name discriminated by
  `observed.edge` and discofetch is deliberately holding the second A
  record until the measurement is trusted — so today gate1 answers
  `reflect.discofetch.link` and gate2 is reached by naming its
  address. When the record lands the flag stops being needed and
  nothing else changes.
- **`--reflect` asks every address a name resolves to**, because
  that is what a vantage is. `NETCHECK-SPEC.md` §2, corrected 31 Aug:
  *"One name, two A records. The client resolves
  `reflect.discofetch.link`, connects to each returned address from
  the same local port with the same `Host`, and reads `observed.edge`
  to know which vantage answered."* Taking only the first address --
  which this did -- asks one vantage and calls it the set.

  And the comparison keys on the **destination**, not the edge name.
  Two vantages can share a name: the same section's cheap intermediate
  is *"a second listen port on gate1 for the same reflect path"*, and
  both ports answer `edge: "gate1"`. Keying on the name would refuse
  that measurement — which means the TCP half is not blocked on a
  second machine at all.
- **`--pin-source-port`, which is what makes two vantages a
  measurement.** `REFLECT-NAT.md` §5 defines the TCP mapping test as
  *"same-local-port connections to reflect through both edges"*, and
  nothing in the tree could bind an outbound source port. Now the
  first fetch reports the ephemeral port it took and every fetch after
  it leaves from that one.

  **It needed no new dependency.** The work was sized against adding
  `socket2`; `tokio::net::TcpSocket` binds and sets `SO_REUSEADDR`
  natively and tokio is already here, so the answer was a smaller
  change than the question.

  Off by default, and it degrades to honesty rather than to a guess:
  a bind that fails is reported (`could not leave from port N`) rather
  than silently falling back to an ephemeral port, and one edge that
  does not answer drops the whole run back to "not a comparison"
  instead of leaving half of one. The verdict label carries its
  caveat — `independent (pinned source port, sequential)` — because
  `SO_REUSEADDR` permits reuse and not two concurrent connections, so
  the fetches are sequential and a NAT may rebind between them.

  Nothing measures anything until there is a **second reflect
  vantage**, which is discofetch's gate2, a box not yet bought.
  This is the DRT half, finished and tested against two local edges
  that echo the source port they saw.

  With one edge it still answers a question worth asking first: name
  that edge **twice** and the run says whether the NAT held its
  mapping across two sequential connections. If it did not, the
  two-edge comparison can never answer `independent` whatever the NAT
  really does — so a second vantage would buy nothing, which is worth
  knowing before buying a box.
- **`tcp_agrees()` called two views of ONE edge an
  endpoint-comparison.** Endpoint-independent means the same external
  port regardless of *destination*, and two connections to one
  destination reusing a mapping is what every NAT does — symmetric
  ones included. So `--reflect URL --reflect URL`, one name typed
  twice, answered `independent`: a symmetric NAT told that it punches,
  which is the most consequential wrong answer this tool can give,
  reached by an obvious command.

  The comparison now requires two *distinct* edges. The same-edge run
  is not discarded but renamed: it measures whether the mapping held,
  which is the precondition for the two-edge test ever working.
- **`tcp_agrees()` called two vantages a comparison, which they are
  not.** Each reflect fetch is its own TCP connection with its own
  ephemeral source port, so two edges report two different ports on
  *every* network — and the old method read that as `Some(false)`,
  "per-destination": a confident statement about a NAT built on
  nothing, in the module whose first premise is that it does not do
  that. Measured live: two fetches to the same edge answered ports
  3075 and 56304.

  It now answers `None` unless the views came from one pinned source
  port, which nothing yet sets, and the evidence line says
  `(separate connections, so separate source ports; not a
  comparison)` rather than naming a mapping. `REFLECT-NAT.md` §5 is
  explicit that the measurement is *"same-local-port connections to
  reflect through both edges"*; pinning one needs `socket2` and is
  `doc/Next.md`'s.
- **The `ssmtp` connector: `host:ssmtp/send`.** `rest`'s sibling, and
  built as one -- `rest`'s scope is an origin allowlist, this one is a
  recipient allowlist, `@example.com` or an exact address, refused by
  name. `full` only, on `rest`'s argument and for `rest`'s reason: it
  links the same TLS stack and adds no new dependency to the profile.

  It carries the part of `rest` worth having. The relay credential and
  the envelope sender live in the scope, so **a program sends mail
  without ever holding the password and cannot forge its From line**.
  That is the argument for this being a connector rather than
  something a guest reaches through `rest`, and it is the whole reason
  it exists.

  Modelled on discofetch's `deploy/mail/df-mail-puller`, which exists
  because "a guest has no SMTP": same relay options, same STARTTLS
  default, same 587, so a deployment can move from that daemon to this
  without re-learning its relay.

  Three-quarters of the work is the part that is not about
  capabilities at all, and each is a test:

  - **Header injection** — a CR or LF in `to` or `subject` ends the
    header and starts whatever came next: a second `Bcc:`, a forged
    `From:`. Refused by name rather than escaped, because a header
    value that wanted a newline wanted something else.
  - **Dot-stuffing** — a body line of exactly `.` ends DATA, so
    without it a guest closes the message early and has the rest of
    its body read as SMTP commands. Bare LF is normalised to CRLF in
    the same pass.
  - **AUTH before TLS** — a scope naming a user with `starttls: false`
    is refused *at startup*. AUTH PLAIN is base64, not encryption, and
    a deployment that would put a password on the wire should not
    discover that at 3am.

  `doc/Next.md` had this as one of four "scopes that decide at call
  time" and that grouping was wrong: a recipient allowlist answers
  *where*, once, at startup, exactly as every scope DRT already had.
  It needed none of the shared-predicate work the other three still
  want.
- **`drt buildinfo` reports the embedded diluvium revision.** It was
  in `BUILDINFO.txt` only -- a sidecar the release workflow writes by
  grepping `Cargo.lock` -- so a binary someone copied off a machine
  could not say which language core was inside it, and a package's
  `requires.diluvium` had nothing in the artifact to check against.
  `doc/Release.md`'s rule is that the compatibility fact travels with
  the bytes, and a fact in a file *beside* the bytes does not.

  Stamped at build time from the lockfile (`crates/drt/build.rs`),
  `unknown` for a build that does not pin diluvium by revision. The
  release workflow now reads it off the artifact like every other
  BUILDINFO field, with the lockfile grep kept as the fallback.

  A **revision**, deliberately, not a version. The core exposes no
  version string at runtime; and the distinctions that have actually
  mattered here -- FM-2 present or fixed, the budget escape open or
  closed -- separate `build12` from `build12p1`, which any semver
  comparison treats as equal because it ignores build metadata. A
  version field would be a field nothing could check.
- **`connectors/ssh` has tests.** It had none, which is how a
  connector that could not answer a single call shipped in v0.3.1 and
  went unnoticed for three days. Five now: scope validation by name,
  the unknown verb, and both halves of FM-3.

### Fixed

- **The `rest` connector handed chunked responses to the guest as
  framing.** `exchange` read to EOF and took everything after the head
  verbatim, so a `transfer-encoding: chunked` response arrived as

      5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n

  where the body was `hello world` — and it was answered `ok`. The
  same silent-corruption shape as the SQL transaction bug: every layer
  above believes the reply.

  Not an exotic path. A server may choose chunked for any response and
  it is the only way to answer without knowing the length in advance,
  which is what nginx does for anything dynamic. `connection: close`
  is why it went unnoticed — read-to-EOF terminates, so all the bytes
  arrive and only their framing is wrong.

  The decoder refuses a frame it cannot read rather than salvaging
  it: a half-decode answered `ok` would be the same bug wearing a
  different hat. Reproduced against a real socket before the fix, and
  pinned by tests covering extensions, upper-case hex sizes, trailers
  and five malformed frames.
- **`netcheck` threw away the address STUN gave it, so the CGNAT rule
  could never fire.** `detect_mapping` returns each probe's reflexive
  address and only `.port()` was kept. `observed_address` stayed
  `None`, the evidence line read "no reflect edge answered" while a
  STUN server had just answered exactly that question — and
  `is_cgnat()` reads `observed_address`.

  CGNAT is the rule that outranks every other in the table, so in the
  only configuration this build supports it could not fire at all: a
  machine behind a carrier NAT with an endpoint-independent mapping
  was told `punchable`, which is the most consequential wrong answer
  this tool can give. The whole table is ordered around that rule.

  The address is now taken from STUN when every probe agrees, and
  only then — two servers reporting different addresses means
  different egress paths, and picking one would be a guess. A reflect
  edge still wins where both exist, because it sees the TCP path too.

  The practical consequence: one two-server run now answers "is this
  network behind CGNAT", with no reflect edge and no TLS stack.
- **`netcheck`'s exit status called a run successful when nothing had
  been asked of the network.** The rule was "fail only if there is no
  UDP mapping *and* no routable v6" -- but `routable_v6()` reads the
  routing table and sends nothing, so a machine holding a v6 address
  whose STUN probes all failed exited 0 with every packet-costing
  evidence line reading `not measured`. The exit status is what a
  script reads.

  It now turns on `probed_anything()`: a UDP mapping, an observed
  address, or an inbound result. `v6-direct` still exits 0, because
  reaching that verdict requires v4 measured and ruled out, and that
  costs a packet.
- **`netcheck` said `not measured` for a failed UDP probe and never
  why.** The error from `detect_mapping` was discarded at the call
  site, so a run against two real STUN servers that answered nothing
  rendered identically to a run with no servers named. Four problems
  with four different fixes -- servers down, name unresolvable, UDP
  blocked on the path, one server given -- looked the same.

  The reason now rides in the evidence line. This is the difference
  between a diagnostic and a shrug, on the one measurement the
  verdict actually turns on.
- **`netcheck` gave `v6-direct` on a network it had measured nothing
  about, and let an inference override a measurement.** Found by
  running the examples on a machine with routable IPv6 — the tree
  being corrected by a network rather than by an argument, which the
  module header says is how it will go wrong first.

  Two faults in one rule. It sat **above** the decisive UDP read, so
  a machine whose UDP mapping measured `independent` — punchable,
  measured, over v4 — was told to use IPv6 instead. `routable_v6`
  reads the routing table and sends nothing, so v6 reachability is
  never measured here at all, and a routable address behind a v6
  firewall is ordinary on consumer gear.

  And its v4 half asked `!has_public_v4()`, which answers `false`
  when no address was observed — so "no reflect edge answered" was
  read as "IPv4 is hopeless", and the tree gave its most specific
  verdict about a network it knew nothing of. Not measured is not a
  finding; that is the one rule this module has.

  The rule now sits below `punchable` and above `relay`, and asks
  `v4_ruled_out()`: an address that came back and is unreachable, or
  a mapping that came back symmetric. With nothing measured the
  verdict falls through to `relay`, which works everywhere.
- **Budgets attenuate at spawn, in both the ways they did not.**
  `Budget::fits_within` and `InstanceConfig::check_attenuation` were
  written, correct, tested, and called from nowhere; `do_spawn` took
  the requested budget verbatim. A child could grant itself more
  instructions and more memory than its parent held. It is now
  refused by name -- `denied`, a reply rather than a fault, the same
  shape as a capability the parent does not hold.

  The second way is the one nobody had named, and it was the cheaper
  escape of the two because it took no intent at all: a child that
  stated *no* budget got `Budget::default()`, which is unlimited,
  under a bounded parent. `budget = nil` is what a spawn request
  looks like when nobody thought about it. An unstated bound now
  resolves to the parent's ceiling, which is what `fits_within`
  always claimed it meant, and a half-stated budget inherits the half
  it did not name.

  `08-spawn-and-hibernation` teaches "a child holds a subset of its
  parent's grants and nothing more". That sentence is now true of
  budgets as well as capabilities.
- **`sql` no longer discards an open transaction silently.** A
  program that opened a transaction and exited without committing got
  `ok` on every call, saw its own writes in-process, and lost them at
  exit with no error and exit 0.

  The connector now rolls back **explicitly**, names the database,
  and the process reports non-zero. SQLite would roll back anyway on
  teardown, so no outcome changes -- what changes is that the outcome
  no longer depends on a connection drop nobody here controls, and
  that the loss is *said*. A silent rollback and an accidental commit
  are both ways leaving it implicit can fail, and only one of them is
  recoverable.

  `drt start` does this as well as `drt run`, at both the places its
  swarm drains. That is the shape a fetchpoint actually runs in, and
  the one where an abandoned transaction is likeliest and least
  visible.

  Note what was **not** wrong, since the ask was written believing
  otherwise: `begin`, `commit` and `rollback` work, and a committed
  transaction survives. The module header saying "autocommit only"
  was the false part, and it is gone.
- **FM-3 is named, and both connectors that had it are covered.** A
  connector reaching `tokio::net` or `tokio::time` under
  `pollster::block_on` panics with "there is no reactor running" --
  `rest` in 0.4.0, `ssh` in v0.3.1, the same bug twice.

  Both fixes were already in; what was missing was any test that
  could fail. Every existing connector test was a `#[tokio::test]`,
  so all of them ran in the one configuration where the bug cannot
  appear -- `rest` had twenty-four. Both connectors now carry a plain
  `#[test]`, and both were confirmed to fail with the fix reverted
  rather than assumed to.

  The subtlety is recorded because the first attempt got it wrong:
  dialing a *closed* port does not reproduce this. The connection is
  refused immediately, the future never pends, and the timeout never
  arms its timer. The test needs something that actually waits.

### Known issues

- **The diluvium pin moves to 5.5.1_build12p1**, which is what
  v0.4.0rc1 deliberately deferred. It carries both upstream fixes
  this repository was waiting on: FM-2's data race (`src/dsync.h`,
  build12) and the instruction budget being switched off by its own
  first firing (`src/dv.c`, build12p1).

  Verified rather than assumed, twice: the whole gate was run against
  this revision before the bump was taken, and the two-line escape
  that ran unbounded on `f137b30` now exits 1.

  **`drt-swarm`'s creation mutex could now be removed** — that was
  always the stated condition, and `grep -A2 'name = "diluvium"'
  Cargo.lock` now shows build12p1. It is kept for this release
  anyway: removing a mitigation in the same change that moves the pin
  it mitigates leaves nothing to compare against if a crash returns.
  The comment at the lock says the condition is met.
- **A guest can hang the whole deployment (FM-4).** One line, needing
  no capability:

      while true do pcall(function() while true do end end) end

  Under `drt start` the deployment freezes -- not the child pinned
  and the rest running, but nothing running: no other instance steps,
  no listener is served. Measured, with the control case (the same
  child without the `pcall`) stopped by its budget in milliseconds.

  The pin now carries build12p1, which fixes the *accounting* half
  -- the hook stays armed, so an escaped instance can no longer
  report perfect health while running on -- and each catch still
  buys `DV_HOOK_STEP` instructions, so a loop of catches is still
  unbounded. Verified against the pinned build.

  DRT cannot close this from here: `dv.h` exposes no interrupt, the
  one hook slot is the budget's, and a CPU-bound guest never returns
  to the host for `dv_exceeded()` to be acted on. The fix is a
  core-file patch upstream. `doc/Failure-Modes.md` FM-4 has the
  operational answer, which is not `Restart=always` -- the process
  never dies -- but a liveness watchdog, and one process per tenant
  you do not trust.
- **The instruction budget is still escapable, and the pin is still
  pre-build12.** Both carried forward from v0.4.0rc1 unchanged; see
  that entry. The budget escape is upstream (`src/dv.c:219`);
  `build12p1` fixes the single-catch case and not the looping one.
- **`crypto/random` is not answered with no config**, and **wasm32 is
  not in the release matrix**, and **`drt ps` is a stub**. All
  unchanged from v0.4.0rc1.

### Upgrading

Two behaviour changes can turn a previously-zero exit non-zero, both
deliberately:

A spawn naming a budget larger than its parent's is now `denied`
rather than granted. If a supervisor relied on that, it was relying
on the bug -- but it will see a refusal it did not see before.

A program that leaves a SQL transaction open at exit now fails. It
was already losing the writes; it just was not told.


## [0.4.0rc1] - 2026-09-01 (prerelease)

`v0.4.0rc1` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

Outbound HTTP from a guest, a NAT diagnostic, and a set of examples
that found a bug in the first of those before anyone shipped it.

**A candidate, not the release.** It is cut so downstream work can
start against something with a tag rather than a branch name, and
it is cut *on the diluvium pin the examples were verified against*
-- `f137b30`, which is pre-build12. Taking the bump and shipping in
one move would mean publishing a set of examples nobody had run the
gate against on those bytes. 0.4.0 proper takes build12 and re-runs
the gate; that is the only difference planned between this and it.

Numbered 0.4.0 rather than 0.3.2 because the connector set changed:
`rest` is new, so `profile.full.connectors` in BUILDINFO is not what
v0.3.1's was, and a package declaring `requires.connectors` is checked
against that list by name. `netcheck` is a new verb on the same
argument.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `rest`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **The `rest` connector: `host:rest/get` and `host:rest/post`.**
  The guest surface is diluvium's, from
  `plugins/rest/rest.plugin.json`, so a program written against
  `diluvium-host` runs unchanged — same calls, same shapes, and the
  same bounds read out of `rest_plugin.c` rather than picked.
  Redirects are not followed, because the C plugin does not follow
  them.

  Unlike the C host's out-of-process plugin, this takes a **scope**:
  an origin allowlist, checked against the URL *and* against the
  resolved address before connecting, since an allowed name that
  resolves into private space is the DNS rebinding shape. An allow
  entry may also carry `headers`, which the connector injects and the
  guest can neither set nor read — so an app calls an authenticated
  API without the program ever holding the credential — and
  `allow_headers`, which when present is the exhaustive set the guest
  may set on that origin.
- **`drt netcheck`.** One of four verdicts — `direct`, `v6-direct`,
  `punchable`, `relay` — with the measurements that produced it, per
  discofetch's `doc/NETCHECK-SPEC.md`. The verdict tree is a table
  rather than nested branches, because it is the part that will be
  wrong first when real home networks surprise us.

  The decisive measurement is the UDP mapping across two STUN
  servers, never the TCP one. A NAT can be endpoint-independent for
  TCP and symmetric for UDP, and reading the verdict off the TCP
  columns would be confidently wrong on exactly the networks where
  this matters most.

  Reflect and the prober — the edges' half — are not implemented
  here; the inbound test reports "not measured" until they exist.
- **`examples/`, a run-through that is also a gate.** Seventeen
  self-contained directories, each run from inside its own folder,
  each carrying an `expected.txt` captured from a real run, and
  `run-all.sh` to diff them. One needs the open internet and is
  skipped unless `--net` says otherwise; a skip is reported as a skip
  and never as a pass. The point of the gate is that this
  repository's own traps list says to re-run the examples when config
  parsing changes — now something does.
- `CHANGELOG.yaml` and `script/changelog.py`, ported from diluvium's, with the release body and the mirror's `changelog.json` generated from one source. CI fails if they drift.
- `doc/Editors.md` (how to get `.dlua` recognised, and why GitHub cannot match the editor extension) and `.gitattributes` mapping `.dlua` to Lua.
- `doc/Next.md`: the deferred work, sized against the code rather than estimated.
- `doc/Verification.md`: what the examples gate cannot reach -- the runs that need a real network, a second machine or a reachable sshd -- written for whoever has one.
- `doc/Ask-0.5.0-Reply.md`: the reply to discofetch's 0.5.0 ask, with the two `reported` findings reproduced and the three decisions DRT needs before starting.

### Changed

- **`drt run` no longer exits 0 for a program that escaped its
  instruction budget.** A guest can catch exhaustion with `pcall` and
  keep running (see known issues); until now `drt run` reported
  success for that, which made it the only place in DRT that hid it
  — `drt start` has always classified such a stop as `exceeded`.

  It is not enforcement and does not pretend to be: the program has
  already run. It is the difference between a budget that was escaped
  and a budget that was escaped silently, and a supervisor can only
  act on the second kind if something says so. A program that stays
  inside its budget is unaffected.

### Fixed

- **The `rest` connector panicked under `drt run`, and never shipped
  that way.** `drt start` drives connectors on a tokio runtime;
  `drt run` uses `pollster::block_on`, which carries no reactor, and
  every socket call needs one. A URL the allowlist *permitted* died
  with "there is no reactor running" and exit 101 — while every
  refusal worked, because refusals are decided before a connection is
  attempted. Found by writing `examples/05` against the connector, in
  the same release that introduced it. The connector now carries its
  own runtime for callers that have none, leaked rather than dropped
  for FM-1's reason.
- **`netcheck` advertised a flag the binary does not accept.** The
  evidence block printed `inbound  not tested (no --port given)`,
  and there is no `--port` in any build -- which also makes
  `direct`, the verdict that requires an inbound connect,
  unreachable from the CLI. It now reads `not measured (no inbound
  test in this build)`, which is true. The flag itself arrives with
  the reflect edges; `09-netcheck` already said so and the program
  now agrees with the example.
- `release.yml`'s publish job copied `install.sh` from one directory above the workspace, which does not exist. Under `set -eu` that failed the step and took the whole publish with it. Never caught because `publish` is the one job a rehearsal does not execute.

### Known issues

- **The embedded diluvium has the FM-2 data race.** This release pins
  `f137b30`, and diluvium 5.5.1_build12 (2026-09-01) names that
  revision and earlier as affected. DRT mitigates it by serialising
  instance creation behind a mutex in `drt-swarm`, so DRT's own
  exposure is closed; anything else embedding this revision is not.
  The pin bump is the real fix, is deliberately not taken in a
  candidate, and is the first thing 0.4.0 does. The mutex stays
  until it lands -- `crates/drt-swarm/src/engine.rs` carries the
  removal condition at the lock, so nobody has to rediscover it.
- **Budgets do not attenuate at spawn.** `Budget::fits_within` and
  `InstanceConfig::check_attenuation` (`crates/drt-config/src/lib.rs:69,114`)
  are written, correct and tested, and are called from nowhere else
  in the workspace. A child takes the budget it names, so it can
  grant itself more instructions and more memory than its parent
  holds. Capabilities attenuate; budgets do not.
  `08-spawn-and-hibernation` teaches "a child holds a subset of its
  parent's grants and nothing more", which is true of capabilities
  and false of budgets. Found independently by discofetch and by
  this repo's own examples pass.
- **A guest can switch its instruction budget off, permanently, in
  two lines.** `pcall` around a loop catches budget exhaustion as an
  ordinary Lua error; the budget never fires again for the life of
  the instance, and `drt run` still exits 0. Measured: exhaustion at
  ~250k steps under a 1,000,000 limit, then an unbounded loop still
  running when killed at 20 s.

  The cause is upstream, at the pin and in diluvium `main` alike:
  the instruction hook (`src/dv.c:219`) clears itself before raising
  -- "once is enough; the error is on its way" -- so a caught error
  leaves nothing armed. **build12 does not fix this**, and it cannot
  be closed from the host side: a CPU-bound loop never returns to
  the host, so there is no resume for `dv_exceeded()` to refuse.
  `doc/Ask-0.5.0-Reply.md` §1.2 is the brief.

  What this release does do is stop hiding it: `drt run` reports a
  non-zero exit for a program that escaped, so a supervisor sees an
  escape rather than a success. That is all a host can do from
  outside the VM.
- **SQL discards an open transaction at exit, silently.**
  `begin`/`commit`/`rollback` do work -- they pass through to SQLite
  on a held connection, and a committed row survives. But a program
  that opens a transaction and exits without committing gets `ok` on
  every call, sees its own write in-process, and loses it on exit
  with no error and no non-zero status. Correct SQLite behaviour on
  a dropped connection; the wrong contract for a durable tier. The
  connector's "autocommit only" header comment is the part that is
  wrong, not the code.

  Decided for 0.4.0, recorded in `doc/Ask-0.5.0-Reply.md` §3.1: the
  connector rolls the transaction back **explicitly**, names it, and
  the instance stops non-zero. The explicit rollback is the point --
  SQLite would roll back anyway on teardown, but leaving it implicit
  makes the outcome depend on a connection drop nobody here controls,
  and an accidental commit is the one failure that is not
  recoverable.
- **No `exec`.** DRT has no local process execution at all — no
  `std::process::Command` anywhere. `exec/run` answers denied and a
  config wiring `connectors.exec` is refused at load. `ssh/exec` runs
  commands on a *remote* host. See diluvium's `doc/DRT.md`.
- **`crypto/random` is not answered with no config**, although
  `doc/HostBaseline.md` names it one of three families every DRT host
  must answer or deny by name. `CryptoScopeType` requires a signing
  key even for the keyless calls, so an unscoped crypto family
  answers nothing. Two deliberate decisions in conflict; unresolved.
- **wasm32 is not in the release matrix.** `drt-web` now has a
  wasm-bindgen export layer and a browser test suite on a branch, but
  the connector/pump layer does not exist, so a program can run and be
  driven in a page and cannot reach `host.fs` or `host.time`.
- **`drt ps` is a stub**, and **the REPL has no line editor**.
- **Budgets bound the VM, not the deployment**: no wall-clock bound, no cumulative spawn bound. `doc/Next.md` sizes both.

### Upgrading

Nothing here removes or renames a surface. `rest` and `netcheck` are
additive and reach nothing unless a config wires them: `rest` answers
no call at all without a scope granting origins, and an empty
allowlist is a startup refusal rather than a runtime surprise.


## [0.3.1] - 2026-08-31

`v0.3.1` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

STUN, the C host's `access` spelling, and two named crashes.

Tagged the same day v0.3.0 shipped and published three days later,
after a repository ruleset on `v*` refused the workflow's App token
with a 403 and the release had to be created by hand.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- **STUN.** `drt stun` serves RFC 5389 binding requests, a `stun`
  config block configures it, and inside `drt start` the same server
  reports its counters to the root program. Two servers on separate
  addresses is what makes mapping classification available at all:
  one vantage says what one vantage saw, and it takes two to know
  whether the mapping *changed*.
- `drt buildinfo --json`, and a `wasm` job in CI that compiles `drt-web` for wasm32 on every push.
- `install.sh` ships as a release asset, so the one-liner has a URL that exists without waiting on server-side work.
- `doc/HostBaseline.md`: the families any DRT host must provide or stub, measured against both hosts rather than reasoned about.
- `doc/Failure-Modes.md`, and `doc/FM-2-Upstream.md` — the brief for fixing FM-2 in diluvium.

### Changed

- **`access` is `read`, not `readonly`** — a compatibility fix, and
  the reason this release is not optional for anyone running the C
  host's configs. `dhost.c` accepts exactly `"read"` or
  `"readwrite"`; DRT accepted `"readonly"` and *refused* `"read"`, so
  an existing discofetch config failed DRT's parse and crash-looped
  under the deploy chain check.
- `BUILDINFO.txt` now says what a binary carries: `dv_abi` (promised since v0.1.0 and never kept) and the connector set per profile, both read out of the artifact by `drt buildinfo` rather than guessed in YAML.
- The README leads with a download that works today. The documented mirror URL 404s — the mirror never grew the `drt` namespace — and a doc that promises a 404 is how that went unnoticed.
- `install.sh` verifies both sources; a mismatch refuses, a missing sums file warns. `DRT_MIRROR` takes `file://` URLs, which is the air-gapped install with no new code.
- `examples/hello.dlua` rewritten against `host.*`, printing byte-identical output to the fifteen-line queue-boilerplate version it replaces. The old one is kept as `examples/by-hand.dlua`.

### Fixed

- **FM-1: a use-after-free in tokio runtime teardown.** `drt relay`,
  `drt stun` and `drt tunnel` dropped a `Runtime` on the way out,
  which races a parked blocking worker's wakeup into a freed
  `Condvar` in tokio 1.53.1. They leak it now.
- **FM-2: a data race in diluvium's continuation registries.**
  `diluvium_shim_addcont` appends to a process-global array with no
  synchronisation, so two threads calling `dv_new` at once can leave
  a slot whose name is NULL and the next scan segfaults in `strcmp`.
  DRT now serialises instance creation behind a mutex. The real fix
  is upstream and is not in this release; DRT's shipped exposure was
  nil either way, since `run`, `start` and `repl` create instances
  only on the drive-loop thread.
- **A relay-only deployment burned a whole core.** With no HTTP listener the drive loop's idle sleep became a spin — 99.7% CPU, forever, on exactly the rendezvous config `examples/rendezvous` documents. 3.0% now.
- The shipped `examples/deployment.json` did not load: the `access` fix above missed the one config file a new user copies. Found by running it rather than reading it.
- Linux non-x86_64 refuses by name in `install.sh` instead of downloading the x86_64 static binary and failing the `--version` guard.
- `doc/Browser.md`'s export table listed fifteen functions and omitted `release`.

### Known issues

- **The embedded diluvium has the FM-2 data race, and this release is
  published.** v0.3.1 pins `f137b30`; diluvium 5.5.1_build12 names that
  revision and earlier as affected. A host that creates instances on
  more than one thread can die in `strcmp` in the first microseconds of
  a fresh process.

  DRT's own exposure is nil in this release for the reason
  `doc/Failure-Modes.md` gives — `run`, `start` and `repl` create
  instances only on the drive-loop thread — so this affected DRT's test
  harness rather than any deployment. Recorded against the version it
  affects rather than only against the one that fixes it.

### Upgrading

**If you run configs written for the C host, this is the release that
parses them.** v0.3.0 refused `access: "read"` and accepted
`"readonly"`, which is backwards. A config edited to say `"readonly"`
as a workaround must be changed back.


## [0.3.0] - 2026-08-28

`v0.3.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

The rendezvous relay, end to end: a real `ssh` session over WSS
through `drt tunnel`, 37 bytes metered, rehearsed against the release
artifact.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Added

- `drt relay`: the rendezvous relay -- parked WSS legs paired by label and spliced, with presence, metering and arbitration reported to the root program.
- `drt tunnel`: SSH over WSS as a dumb pipe, in three shapes -- the OpenSSH `ProxyCommand` contract over stdio, a WS→TCP listener, and the device side of the relay.

### Known issues

- `access` is spelled `readonly` here and refuses `read`, which is backwards from the C host. A config written for `dhost.c` does not parse. Fixed in 0.4.0.
- A relay-only deployment (no HTTP listener) spins a core at 99.7% forever. Fixed in 0.4.0.
- `drt relay`, `drt stun` and `drt tunnel` can SIGSEGV on clean shutdown (FM-1). Fixed in 0.4.0.


## [0.2.0] - 2026-08-27

`v0.2.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

Park mode: the device corner of the triangle.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`


## [0.1.0] - 2026-08-27

`v0.1.0` &middot; dv ABI 1 &middot; diluvium `f137b308c4dc`

The first release: DRT as a static binary that runs sandboxed Lua
under a capability model and serves fetchpoints.

### Connectors

- `full`: `time`, `fs`, `crypto`, `sql`, `ssh`, `listen`
- `slim`: `time`, `fs`, `crypto`, `listen`

### Known issues

- `BUILDINFO.txt`'s header promised a `dv_abi` field it did not emit. Kept from v0.1.0 in 0.4.0.
