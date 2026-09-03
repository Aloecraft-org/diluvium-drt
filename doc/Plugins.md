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
that exists: `drt_swarm::pump` (doc/Wasm.md M3), written for the browser
and merged here 2026-09-03. This assessment was made against it as a
branch; nothing below waits on it any more.

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
- **The pump defers.** `Dispatcher::route` returns a
  `PendingCall` that owns everything the connector needs, and
  `drt_swarm::pump::Pump` polls its future once per pump with a no-op
  waker, parks a pending one in an in-flight table, holds a reply owed to
  a hibernated instance and drops one owed to a dead instance. Written
  against the wasm branch when this was assessed and merged here since
  (doc/Wasm.md M3, landed 2026-09-03), so what follows can be built
  against it rather than sequenced after it. Before it, every hostcall
  path blocked inside the drive step and one slow connector stalled every
  instance (`doc/Failure-Modes.md`, the `rest` case).

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
  kept, absolute path, no `PATH`), a reader thread pushing reply frames
  over an mpsc into the drive loop. That is the bridge shape the relay,
  STUN and http listener already use, and it needs no tokio, so the
  no-reactor failure class cannot reach it. A browser `Channel` later is
  a JS function or Worker speaking the same frame bodies.
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

The in-flight table (`drt_swarm::pump`), keyed by instance and token with
the future parked beside it, is the plugin ledger with one more column
for the wire id. `max_inflight`, per-call deadlines, sweeping entries when
an instance dies, and the `wake` policy all hang off that table. The C
host declares `wake` and consumes it nowhere; DRT has real hibernation,
so it can honour it. Two tables for the same thing would be the mistake.
A blocking `PluginConnector` in the style of `rest` is a fair stopgap for
`drt run` but not for a deployment.

## 4. Per target

Natively, the process transport. On wasip2 there are no threads and no
subprocesses, so the process transport is cfg'd off and plugins are
honestly native-only in the first cut; the eventual portable transport is
a wasm component, which SPEC.md §7 already names as the only place WIT
enters. In the browser the transport is a JS function or Worker, and it
depends on the deferred pump exactly as `fetch` does.

The order, now that the wasm milestones have landed here: codec, manifest
and process channel first, testable against `plugin_echo.c` with no
drive-loop change; then config, the `.host.lua` key and
`capabilities/list`; then the in-flight integration, which no longer
waits on anything. The JS-host bridge files this assessment expected
`drt-web` to delete in M7 are gone, so a browser transport starts from
the terminal contract in `doc/Browser.md` instead: the module imports
nothing but wasm-bindgen's glue, and a JS function or Worker on the other
side of that boundary is the transport.

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

The first plugin, and the reason to build the channel now, is the
browser capability: `doc/Playwright.md`.
