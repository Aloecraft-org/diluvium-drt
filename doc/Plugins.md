# Plugins: adding a hostcall family without a release

**Status:** assessment, written 2026-09-03 against v0.4.2, before any of
it is built. The decisions in §5 are open and the owner's. Where a claim
rests on code it names the file.

**The ask.** Add a hostcall capability after the fact, without building a
new `drt`. Distinguish the connectors compiled into the binary from ones
connected later, and define the interface the later ones use.

---

## 1. The verdict

Implement the plugin channel the C host already specified
(`doc/BUILD8.md` §2 in diluvium) as a new connector *backing* behind the
existing `Connector` trait. Do not design a second protocol. The one
piece of real engineering is a hostcall pump that can defer a reply, and
that is the deferred pump on `claude/drt-wasm-port-planning-4ua6qk` (M3).

Rust's lack of dynamic linking is not the obstacle it sounds like. The
release artifact is static musl, so `dlopen` was never available, and an
in-process native plugin would share the address space and freeze a Rust
ABI. The C host rejected in-process plugins for the same reasons; its
README says that if a plugin ever needs a header, the channel has become
a C API. Subprocess plus msgpack is the better design, not a workaround.

## 2. What is already in place

- **The trait is the abstraction.** Capability gating, token echo and the
  answered-always guarantee live in the dispatcher once
  (`crates/drt-connector/src/lib.rs`). A plugin is one more `impl
  Connector` and never sees an ungranted call. The mock backing already
  proves a guest cannot tell backings apart; the plugin backing inherits
  that acceptance test.
- **The protocol exists, argued through.** Exec an absolute path, hand it
  a socketpair as fd 3, speak `u32` big-endian length-prefixed msgpack
  frames. A request is `{version, id, target, args}`; a reply is
  `{version, id, final, value}` or `{version, id, final, error: {class,
  code, message}}` with `class` one of `transport`, `plugin`,
  `capability`. The manifest `<name>.plugin.json` holds flat metadata the
  host reads (`exec`, `checksum`, `transport`, `max_inflight`,
  `call_timeout_ms`, per-capability `name` and `wake`) and schemas it
  skips. The hard calls were made there: the wire id is host-global
  because guest tokens collide across instances; stdout is not the
  channel because one stray log line desyncs framing forever. Fixtures
  exist: `test/plugin_echo.c` (the protocol from scratch, no headers) and
  `plugins/rest/rest_plugin.mjs` (the same frames over fd 3 under Node, or
  over `postMessage` in a Worker).
- **Config half-expects it.** `ConnectorWiring.backing`
  (`crates/drt-config/src/lib.rs`) is declared for a build carrying more
  than one backing, and nothing reads it. The `.host.lua` loader refuses a
  `plugins` key by name today (`crates/drt/src/config.rs`), so a C-host
  deployment that wires plugins does not load on DRT: the one gap in the
  "swap the binary, edit no files" commitment.
- **The menu is designed and unbuilt.** SPEC.md §5 keeps
  `capabilities/list`; nothing in DRT's Rust answers it. The C host's entry
  shape is `{name, kind, owner, granted, visibility}`. That listing is
  where a program sees the builtin/plugin distinction.
- **The pump defers, on the wasm branch.** `Dispatcher::route` returns a
  `PendingCall` that owns everything the connector needs, and
  `drt_swarm::pump::Pump` polls its future once per pump with a no-op
  waker, parks a pending one in an in-flight table, holds a reply owed to
  a hibernated instance and drops one owed to a dead instance. On `main`
  every hostcall path still blocks inside the drive step, so one slow
  connector stalls every instance (`doc/Failure-Modes.md`, the `rest`
  case).

## 3. The shape

### 3.1 Builtin and plugin, defined once

A **builtin** is compiled in, gated by a cargo feature, wired by the
static match in `wire_connectors`, listed by `drt buildinfo`. A
**plugin** is declared by a manifest, wired by a `plugins` block at
startup, listed by `capabilities/list` with `kind = "plugin"` and its
owner. One registry, one dispatcher, one resolve-by-family; a second
dispatch path would be the second runtime the README forbids.

The distinction shows in three places and nowhere in behaviour:

- **Config.** `plugins` beside `connectors`, not inside it, so a typo in a
  connector name still fails by name instead of becoming a plugin lookup.
  A `.host.lua` `plugins` block maps field for field: `manifest` resolves
  beside the config file, `max_inflight` and `call_timeout_ms` override
  the manifest's.
- **`capabilities/list`.** Kind and owner, per the C host's shape.
- **`buildinfo`.** It keeps describing the binary: a line naming the
  plugin *transports* the build carries (`process` natively), never the
  plugins a deployment wires, because that is a deployment fact.

It must not show in behaviour: same denial wording for an unwired
family, same status vocabulary, same token echo.

### 3.2 A `drt-plugin` crate, target-neutral

- Manifest types reading only flat metadata, the way the C host reads it.
- The frame codec as serde types, the way `drt-hostcall` does the guest
  encoding.
- `PluginConnector: Connector`, over a `Channel` trait with the platform
  code in leaf impls. `ProcessChannel` natively: fork and exec with the
  discipline `connectors/exec` already has (own process group, fd 3
  kept, absolute path, no `PATH`), and the frames driven by a polled
  state machine over the non-blocking socket, stepped from the drive loop
  (§4.1) -- not a reader thread, because the same state machine has to
  run where there are no threads. It needs no tokio, so the no-reactor
  failure class cannot reach it. A `tcp` channel obtains its stream by
  dialing instead of forking and shares everything else; a browser
  `Channel` later is a WebSocket or a Worker speaking the same frames.
- Error mapping is fixed by the wire: a `value` becomes `Reply::ok`; an
  `error` becomes `Reply::error` with class, code and message folded into
  `detail` exactly as `dhost_plugin.c`'s `fail_call` spells it, so
  `host.try` prints the same sentence on both hosts. `denied` stays the
  dispatcher's and is never a plugin's to say.
- The `Channel` trait takes the `MaybeSend` shape `engine.rs` uses, for
  the same browser reason.

### 3.3 One extension: a scope

The C protocol has nowhere to put a scope, so `rest` as a plugin there
was an unscoped outbound-HTTP primitive; `connectors/rest/src/lib.rs`
makes the argument. DRT's `plugins` block can carry `scope`, the manifest
can declare whether the plugin takes one, and a hello frame at startup
delivers it, additive under the existing `version` field and answered ok
or error so an ill-scoped plugin still fails at startup by name. This is
the one place to extend the protocol rather than adopt it.

### 3.4 Built on the deferred pump

The in-flight table on the wasm branch, keyed by instance and token with
the future parked beside it, is the plugin ledger with one more column
for the wire id. `max_inflight`, per-call deadlines, sweeping entries when
an instance dies, and the `wake` policy all hang off that table. The C
host declares `wake` and consumes it nowhere; DRT has real hibernation,
so it can honour it. Two tables for the same thing would be the mistake.
A blocking `PluginConnector` in the style of `rest` is a fair stopgap for
`drt run` but not for a deployment.

## 4. Per target, and the transport that decides it

The first draft of this section said plugins were native-only because a
plugin is a process and wasip2 cannot spawn one. That is true and it is
the wrong conclusion. The protocol is frames over a byte stream; the
socketpair on fd 3 is one way to *obtain* the stream, and it is the
only part of the design that needs a fork. Separate the two and plugins
run everywhere the stream does.

### 4.1 The transports

| transport | obtains the stream by | who starts the plugin | works on |
|---|---|---|---|
| `socketpair` | DRT forks, execs an absolute path, hands over fd 3 | DRT, per deployment | native unix; the C host's, kept for its manifests |
| `tcp` | DRT dials an address, loopback by default | the operator: a service, a sidecar, a Windows service | native unix, native windows, **wasip2 under wasmtime** |
| websocket | the page dials | the page | the browser tier, later |
| worker | `postMessage` | the page | the browser tier, later |

The frame bodies are identical on every row, which is the claim
`rest_plugin.mjs` already makes for two of them. A plugin written for
fd 3 becomes a `tcp` plugin by changing where it reads and writes and
nothing else.

**`tcp` is the one that reaches wasip2, and it is measured, not
inferred.** `doc/Wasm.md` §2.2 recorded `std::net` binding under
`wasmtime -S inherit-network=y -S tcp=y`, and M6 (`ee22ed6` on the wasm
branch) serves a deployment's listener from wasmtime through
non-blocking `std::net` sockets stepped from the drive loop, with no
`wasi:io/poll` crate and no threads. The plugin channel is that
acceptor's client half: a blocking `connect` at startup, so a plugin
that is not there is a refusal by name before anything runs; then
`set_nonblocking`, and one small state machine per plugin -- write
frames with the partial write retained until the socket takes the rest,
read frames as their length prefix completes, hand each reply to the
in-flight table -- polled every tick the way M6 polls its connections.
BUILD8 §2.6's non-blocking descriptor with partial-write retention is
already this design; the C host arrived at it for backpressure, and it
is also what a target with no threads needs.

That polled state machine is also the right native implementation. A
socketpair is a byte stream too; set it non-blocking and the same code
drives both transports, and the reader thread §3.2 proposed goes away.
One `FrameStream` over any non-blocking `Read + Write`, two ways to
obtain one today, and the browser's later.

### 4.2 What it buys

- **Windows without a native `drt`.** wasmtime runs on Windows, and
  `drt.wasm` under it carries the `wasi` profile. Every capability that
  profile lacks can be a plugin: a Node program, a Python program, a
  native helper, listening on loopback. That is the whole of the ask
  answered, if `wasi:sockets` under wasmtime on Windows behaves as it
  does on Linux -- expected, not measured, and the first thing to
  measure.
- **The connectors that cannot cross come back.** `rest`, `ssmtp`,
  `ssh`, `tunnel`, `relay`, `stun` and `netcheck` are gated off wasm by
  tokio (`doc/Wasm.md` §2.1). Served as plugins by a native process,
  they reach a wasm `drt` unchanged and unported. Upstream's C
  `rest_plugin` and its Node twin are such a process already.
- **A future native Windows build needs this transport anyway.** There
  is no fd 3 on Windows; a Windows `drt` would speak `tcp` to its
  plugins from the first day.

### 4.3 What it costs, and what changes

- **The security posture moves, and the doc has to say so.** BUILD8 §2.7
  ships no plugin authentication because a socketpair has no other end:
  parentage is structural. A loopback port has other ends -- every
  process on the box -- so the `tcp` transport needs the one thing the
  socketpair did not: a shared secret, named by the `plugins` block
  (`secret_env` or `secret_file`, the way `crypto` names its key),
  presented in the hello frame of §3.3 and checked by the plugin before it
  answers anything else. Bind to loopback by default; a non-loopback
  address is a deliberate act the config spells out, and it is a network
  surface in GUARANTEES.md's sense.
- **Lifecycle is the operator's.** DRT started and reaped its socketpair
  plugins; a `tcp` plugin is a service something else supervises, which
  is the division SPEC.md §13a already draws for the process itself. A
  plugin that goes away mid-call answers `transport`-class errors to the
  calls in flight, which the C host's error classes were designed for;
  DRT reconnects with backoff and refuses by name if the plugin never
  returns.
- **The hello frame stops being optional.** It carries the secret and
  the scope, and it is the one addition to the C protocol. A plugin
  written against BUILD8 §2 that does not know it answers `bad_target`,
  which DRT reads as "no scope, no secret": legal for a `socketpair`
  plugin, refused for a `tcp` one.
- **The manifest grows one value.** `transport` already exists; `"tcp"`
  joins `"socketpair"`, and the address lives in the deployment's
  `plugins` block, not the manifest, because where a service listens is
  the operator's fact. The C host refuses an unknown transport by name,
  so a `tcp` manifest does not silently load there.

### 4.4 Sizing the wasip2 half

| piece | size |
|---|---|
| the `FrameStream` state machine over a non-blocking stream, shared by both transports | ~2 days, with M6's polled connection as the model |
| `tcp` in config and manifest, the hello frame with secret and scope, reconnect | ~1 day |
| a wasmtime test: the echo plugin as a loopback service, a `drt.wasm` calling it through the wasip2 gate the wasm branch already runs in CI | ~1 day |
| the Windows measurement: wasmtime plus `-S tcp` on a Windows runner, one echo round trip | ~half a day, and it decides whether the claim above is true |

On top of §3's channel work, not instead of it; the codec, manifest and
`PluginConnector` are shared.

### 4.5 What stays impossible under wasmtime, so nobody plans around it

- Spawning. wasip2 has no process API, so `socketpair` plugins and
  `exec` stay native-only. The plugin has to be running already.
- Loading a second component at runtime. A composed component --
  `drt.wasm` linked with a plugin component through `wac` -- is the route
  where WIT enters (SPEC.md §7), and it is static: the operator composes,
  the result is one component. It serves pure-logic plugins that can live
  inside WASI; it cannot serve a browser or anything else that needs a
  process. Later, and separately.
- The browser tier. A page has no TCP; its transports are the WebSocket
  and the Worker, with the same frames.

The safe order against the wasm branch is unchanged: codec, manifest and
`FrameStream` first, testable against `plugin_echo.c` over a socketpair
with no drive-loop change; then config, the `.host.lua` key and
`capabilities/list`; then the in-flight integration and the `tcp`
transport after M3 and M6 merge, since both are their code. The JS-host
bridge files `drt-web` deletes in M7 are not a foundation for a browser
transport.

An acceptance test comes free: the same guest against builtin `rest` and
against upstream's C `rest_plugin` wired as a plugin, identical replies.
That is the `cap7_plugins` slice finally in scope.

## 5. Decisions, open

1. Adopt BUILD8 §2 verbatim, or a DRT-native protocol. Recommendation:
   verbatim; C-host compatibility is this repo's first priority and the
   fixtures exist.
2. The scope hello frame, or unscoped plugins as the C host has them.
3. Whether a package's `requires.connectors` may be satisfied by a
   plugin, or whether `buildinfo` stays strictly about the binary and the
   deployment's answer is `capabilities/list`.
4. Sequencing: plugins wait for M3, or the deferred pump is pulled forward
   as shared work.
5. Whether `tcp` ships in the first cut beside `socketpair`, or after it.
   Recommendation: beside it. The polled `FrameStream` serves both, and
   `tcp` is the transport every target after unix needs.

The first plugin, and the reason to build the channel now, is the
browser capability: `doc/Playwright.md`.
