# DRT — the Diluvium RunTime

**Status:** founding spec for the `diluvium-drt` repository. Decisions here were
made deliberately; where something is open it says so. Drive fast: the
milestone that matters is **demonstrable in Lab ASAP**, and every future
direction in this spec is present as a *seam*, never as a blocking dependency.

## 1. What DRT is, and the split

**Diluvium** (the existing C repo, `aloecraft-org/diluvium`) becomes purely the
language: a 100%-backward-compatible extension of Lua with its REPL, compiler,
analyzer, guest libraries, and the `dv.h` instance ABI as its embedding
surface. Run standalone, it is Lua-with-more: full stdlib, `require`, no
sealing, no surprises. It *loses* the swarm layer (`dvs.*`) and the generic
host (`host/`) — those move here.

**DRT** (this repo, Rust) is the runtime environment: swarm, transparent host
with root config, hostcall connectors, orchestration (ego-proc), transport
(ego-transport), observability, and the extended/attachable REPL. DRT *embeds*
diluvium via FFI; installing DRT gets you everything. The two are concentric,
not parallel: there is never a second runtime with different behavior for the
same program.

Naming: DRT is the working name (lean over DRE). The language binary stays
`diluvium`; this repo's binary is `drt`. `.dlua` is the source extension for
files using Diluvium syntax; `.lua` is accepted everywhere, forever.

## 2. The contracts (read these files first)

Attach/clone `aloecraft-org/diluvium` for reference. The normative surfaces,
in priority order:

| contract | where | notes |
|---|---|---|
| Instance ABI, v1 | `src/dv.h` | The whole FFI. Four rules in the header comment: bytes in/bytes out (msgpack only), one instance one thread, the host drives (no scheduler/clock inside), version first. `DV_ABI_VERSION` covers the ABI + msgpack ext registry + snapshot format *together*. |
| Swarm semantics | `src/dvs.h`, `src/dvs.c` | The behavior DRT's swarm crate reimplements. `dvs.c` depends on `dv.h` + a msgpack codec and **nothing else** — verified. It stays in diluvium, frozen, as the differential-test reference until DRT's swarm passes acceptance; then diluvium deletes it. |
| Hostcall encoding | `doc/Hostcall.md` | Request `{tok, call, args}`, reply `{tok, status, value|detail}`; statuses ok/denied/error/malformed; every drained request is answered; token echoed verbatim; unknown status = error (growth without version break). Copy this doc into DRT as normative text; diluvium references it. |
| Host protocol duties | `doc/Host.md` | Construction, drive loop, roster, queue pump, hostcalls. Acceptance test: **a guest must not be able to tell hosts apart.** |
| Capability direction | `doc/Capabilities.md` | effect × capability × scope; one config shape at every depth; the host is the root's parent; host-config and spawn-request are the same object. DRT is the first implementation of this model — do NOT port the `.host.lua` shape. |
| Existing Rust bindings | `bindings/rust/` | `diluvium-sys` (partial transcription — see §4), safe `diluvium` crate (`Send + !Sync`, `rmp-serde` with `to_vec_named` — field names cross the boundary), `diluvium-wasmtime` (core as wasm module; note its Cargo.toml comments on sjlj/EH). |
| Acceptance suite | discofetch repo, `capability_testing/` | cap1–cap7 slices driving the real host (grown past the cap1–cap5 this spec was written against: `cap6_fs` and `cap7_plugins` were added since). Port into this repo as integration tests; passing them rewired to DRT is the definition of "usable". |

## 3. Workspace layout

```
diluvium-drt/
  crates/
    drt-config      # manifest/config schema — serde types are the SOURCE OF TRUTH;
                    # LuaCATS defs are GENERATED from them, never authored
    drt-caps        # capability grammar: effect×capability×scope, pattern match
                    # (host:fs/*), attenuation check, provenance chain (see §6)
    drt-hostcall    # the encoding as serde types: request/reply, status enum,
                    # token allocation (guest lib allocates from 2^30 up)
    drt-connector   # Connector trait, registry, capability gating, mocks
    drt-swarm       # the dvs port, over diluvium-sys; Engine seam
    drt             # the binary: run | start | repl | ps (see 13a)
  connectors/       # time, fs, sql, listen, exec, ssh — each feature-gated
```

Consumed: `diluvium-sys`/`diluvium` (from the diluvium repo — they STAY there,
versioned with the header; see the factoring notes), `ego-proc`,
`ego-platform`, `ego-transport`.

Targets: native (linux glibc+musl, mac, windows, arm), `wasm32-wasip2` (run
under wasmtime), `wasm32-unknown-unknown` (browser, wasm-bindgen). **No WIT
anywhere in v1.** Browser and wasip2 are separate builds with platform code
confined to leaf adapters (ego-platform's job). Build profiles: `slim` (run a
script, fs/time/stdio only) and `full` are cargo-feature profiles of one
codebase; the embedding library is the degenerate profile.

## 4. Spike zero (before anything else)

Validate linking C-core wasm objects (setjmp/longjmp lowered onto the EH
proposal via `-mllvm -wasm-enable-sjlj`, wasi-sdk) together with rustc-emitted
wasm, on both wasm targets. This is the only critical-path item that could
force a design change. `bindings/rust/diluvium-wasmtime/Cargo.toml` documents
the EH scar tissue. If same-module linking fails, fallback is the nested-module
shape (diluvium-wasmtime) on wasip2 and the existing JS-host pattern in browser
— acceptable, but know which world you're in before building.

Then: **complete the `diluvium-sys` transcription** (upstream, in the diluvium
repo). Present today: new/free/load/last_error, queue family, run/resume/
waitset_get, set_notify. Missing: `dv_set_budget`, `dv_usage`, `dv_memory`,
`dv_exceeded`, `dv_snapshot`, `dv_restore`, `dv_register_code`, `dv_layout`,
and the endpoint family (`dv_endpoint_allow`, `dv_set_endpoint_handler`,
`dv_endpoint_queue`, `dv_endpoint_close`). The swarm port is impossible
without snapshot/budget/endpoints.

## 5. Config: one shape at every depth

- The root config is a property of the OS process (file + flags + env, merged
  into one root object). It holds: the root program, the capability ceiling,
  budgets, connector wiring (scopes), transport listeners, identity.
- **Host-config and spawn-request are the same serde object.** Attenuation is
  the only rule: a child's config must fit inside its parent's, checked
  identically whether the parent is the process or another instance.
- A grant is `effect × capability × scope`. Scope-types are declared per
  capability in a registry; a malformed or ill-scoped grant fails **at
  startup, by name** — never as a mystifying `denied` at first call.
- Scopes stay host-side (a directory for fs, a directory for sql, a key);
  programs name resources *within* granted scopes. Config never carries the
  application's filenames.
- Supervisor programs are **optional**: config + one program is a complete
  deployment (DRT drains the root's lifecycle queue itself). Dynamic lifecycle
  policy remains programs — never Rust config (see §8).
- Menu visibility vs. grants stays two questions (`capabilities/list` marks
  each entry granted or not; public by default).

## 6. Capabilities: inspectable means provable

`drt-caps` owns the grammar (`host:fs/*` covers call names the way `queue:*`
covers queues — same semantics as `dvs_holds`/`dvs_may_grant`, differentially
tested against them) plus what the C layer never had: **provenance**. Every
instance's set records who granted it, attenuated from what, back to the
process root. One introspection surface serves it all — caps, budgets, usage,
queue depths, residency, health — behind `drt ps` / `drt caps <id>`, a
grant-gated hostcall, and (later) Lab. SSH principals (§9) are nodes in the
same tree.

## 7. Connectors

One trait, several backings, zero distinctions at the call site:

- A connector is an ordinary Rust impl: typed via serde (args deserialize into
  a struct, return serializes into the reply). The dispatcher does capability
  gating, token echo, and the answered-always guarantee *once*.
- Platform-specific connectors (e.g. a wasm-bindgen browser connector) are
  `#[cfg(target)]`-gated impls behind the same registration — the ego-platform
  pattern. Config names a connector; the registry resolves it to whatever
  backing this build carries.
- Dynamic loading is a seam, not v1: native → wasm component (WIT enters only
  here, only later); browser → a JS function handed in at init behind a
  `JsConnector` adapter.
- Mocks implement the same trait. Guests cannot tell — that indistinguishability
  is load-bearing (prototype against mocks, deploy against real, guest
  unchanged) and is the acceptance test.
- v1 set: `time`, `fs`, `sql`, `listen` (over ego-transport), `exec` (behind a
  loud flag — granting it is leaving the sandbox, the instruction budget cannot
  bound it), `ssh` client (`host:ssh/exec`, scoped `{host, user, key}`, exec's
  caveats verbatim).
- Hostcall metering (open in `doc/Hostcall.md`): settle it here as host-side
  arithmetic — charge per message and per byte at the queue layer. Not a
  format change.

## 8. Swarm

Port `dvs.c` semantics faithfully; the six owned things are exhaustive
(instance table, parentage, per-instance caps, lifecycle drain, budget
enforcement, snapshot cache + wake_on_message). **There is still no supervisor
type**: restart/backoff/topology for *guest agents* are programs holding the
lifecycle capability. `OrchestrationStrategy` never touches guest agents.
Semantics to preserve exactly (differential-test against `dvs.c` via the
ported cap suite — pragmatic, not ceremonial; the suite passing is the gate):
attenuation-only grants, subtree kill, self-initiated hibernation (a program
parks after pushing `{op="hibernate"}`; nothing swaps an instance out behind
its back), the four-row `dvs_push` delivery table, bounded wake buffer
(DVS_LIMIT, never growth), spawn rate limit, 32KB request cap.

New in DRT: a **snapshot store trait** with a directory-backed impl in v1
(durable agents — snapshots survive the process; it is bytes to files and it
forces the ref encoding right). Identity stamping as in `dv_snapshot`'s
`host` arg; the stamp may be derived from the SSH host key (§9).

**Engine seam:** `drt-swarm` reaches instances through an `Engine` trait ("a
thing that produces instances speaking dv ABI vN"). v1 ships exactly one impl:
current diluvium, statically linked. The second impl — the core as a wasm
module under wasmtime, building on `diluvium-wasmtime` — is deliberately
deferred and pays twice when it lands: multi-version support (each diluvium
version is a `.wasm`; no symbol collisions) and a strong-isolation tier
(untrusted bytecode inside a wasm sandbox — real mitigation for the core's
missing bytecode verifier). Dropping pre-ABI diluvium versions is free:
`dv_*` exists only from 5.5.1_build3; there is nothing older to host.

## 9. ego-proc integration, REPL, SSH

- **ForeignActor adapter**: instances become actors via drive/deliver/drain/
  health/passivate/reactivate (see the factoring notes — the trait itself
  belongs in ego-proc). `on_tick` = drive until parked or slice spent;
  park-with-timeout becomes an orchestrator timer (DRT owns no clock — the
  contract finding its home). Health is measured, not self-reported:
  saturation from `dv_usage` against `dvs_budget`-equivalent.
- **Connector/service actors**: listener, sql pool, crypto, snapshot store —
  supervised by ego-proc. Restart strategies are legitimate *here* because a
  connector restart is semantically invisible to guests (in-flight tokens get
  `status="error"`, which correct guests already handle). One ActorHealth
  shape for both populations; one `drt ps`.
- **REPL is an instance, not a mode**: a sealed guest that `load()`s input,
  bridged to a terminal/browser/socket via its queue pair. `drt repl` = that
  instance with a generous local grant (connectors become interactively
  explorable). **Attach** = the same instance wired into a live deployment
  with caps granted at attach time. Per-instance injection = attach + direct
  queue access + per-instance connector mocks.
- **sshd/sshc**: russh for the protocol (never hand-rolled from RustCrypto
  primitives), pubkey-only, modern suite only (ed25519 + curve25519 +
  chacha20-poly1305, strict kex). Host key = node identity; authorized keys
  map to capability grant sets in root config (an SSH principal is an
  attenuated node in the provenance tree). PTY channel → REPL attach;
  subsystem channel → framed msgpack for programmatic access. sshd and REPL
  attach are one milestone wearing two names. The transport itself lives in
  ego-transport (see its brief); DRT consumes `ssh://` like any scheme.

## 10. Endpoint refs (the distribution seam)

Refs are opaque to guests (msgpack ext 0x02; guests cannot parse them — any
encoding change is invisible to every guest ever written). DRT mints refs as a
small tagged encoding `{scheme, address, identity}`, resolved at bind time;
`local` is the only scheme implemented in v1. This is mandatory now, not
later, because **refs are captured inside snapshots**: durable agents mean a
snapshot restored in another process/machine next week, and process-local
indices would make every stored snapshot untranslatable. Non-local schemes
resolve through ego-transport when distribution lands — additive, no format
break.

## 11. Determinism seam & the guarantees doc

Structure the drive loop around an event log (which-queue-fired decisions +
hostcall replies — the only two nondeterminism sources). Replay is a later
feature flag, not a rewrite. Do not advertise replay until implemented.

Maintain `GUARANTEES.md` from day one, CI-checked for presence: the C core
remains C (Rust wraps the part that was never the dangerous part — say this
sentence); the bytecode verifier does not exist yet (treat untrusted bytecode
as untrusted native code; `DV_FLAG_TEXT_ONLY` and the wasm-engine tier are the
mitigations); exec leaves the sandbox and the budget cannot bound it; sshd is
a deliberate front-door exposure; tokio introduces scheduling nondeterminism.
The list only grows deliberately.

## 12. v1 cut and acceptance

**In:** workspace above; ego-proc adapters; ego-transport-backed listener;
connectors time/fs/sql/listen/exec/ssh-client; directory snapshot store; ref
encoding; introspection + `drt ps`; REPL local; sshd + attach (the Lab-demo
milestone); GUARANTEES.md.
**Seams only:** Engine (one impl), event log (no replay), dynamic connector
loading, non-local ref schemes.
**Out, captured:** WIT/components, replay, multi-machine endpoints, Lab UI
(but introspection is designed as Lab's backend), multi-engine.

**Acceptance:** the ported capability suite passes against `drt start`; guest
indistinguishability holds (same guest, mock vs real connectors); `drt repl`
locally and `ssh`-attach against a live swarm — that demo is the finish line.

## 13a. The command surface (settled 2026-08-26)

`drt` is **one binary that is both the runtime and its own client**, like
`git` — not a client plus a daemon holding a registry of deployments, like
`docker`. The decisions, and why:

**One process, one deployment.** §5 already fixes it: the root config is a
property of the OS process, so one process is one root config is one swarm.
Two deployments are two processes. There is no supervising `drtctl` that owns
several, and that is the same refusal as §8's: this project has no supervisor
type, and process supervision is systemd/docker/k8s's job, done better.

**`drt start`**, replacing the working name `serve` — it pairs with `stop`,
where `up` would imply the compose-style multi-service orchestration this
deliberately is not. It runs **in the foreground**, logs to stdout, and exits
on SIGTERM: the twelve-factor shape, so a process supervisor can supervise it.
There is deliberately **no `--detach`**.

**Port binding is opt-in, never implied by the verb.** `start` binds only what
the config's `listeners` name. A config with none is a complete, legitimate
deployment that binds nothing — a headless swarm driving its root program.

**The client commands reach a running deployment over a transport endpoint** —
the §9 sshd subsystem channel carrying framed msgpack, which is the same
mechanism serving REPL attach and remote access. One mechanism, three uses. A
deployment exposes a local control endpoint derived from its name, so `drt ps`
with no arguments finds the obvious local one and `drt ps ssh://host:2222`
reaches any other. N parallel deployments are therefore N processes and N
endpoints, with no registry invented to track them. (If enumerating local ones
is ever wanted, the honest form is a filesystem convention — control endpoints
under one directory — and never a daemon.)

**Verbs, and what each may honestly mean:**

| verb | what it does |
|---|---|
| `run <program>` | one program to completion, foreground. No listeners, no control endpoint. |
| `start` | the deployment. Foreground; binds what the config names. |
| `stop` | graceful: stop accepting, hibernate everything parked into the snapshot store, exit. This *is* the durable-agents story, so it earns v1. |
| `ps` / `caps <id>` / `status` | introspection against a running deployment (§6). |
| `repl` / `attach` | the REPL instance, local or wired into a live deployment (§9). |
| `pause` / `resume` | **named, not built in v1.** See the caveat below. |

**The `pause` caveat, because it is a doctrine trap.** `pause <instance>`
must not mean "suspend that agent": §8 is emphatic that hibernation is
self-initiated and nothing swaps an instance out behind its back. What is
legitimate is what `dvs_hibernate` already allows — hibernating a *parked*
descendant, which is safe precisely because the descendant chose the moment by
parking. So `pause <id>` may only mean "hibernate it if it is parked, and
refuse by name if it is not." Deployment-wide `pause` (stop stepping, e.g. to
take a consistent snapshot set) is unobjectionable but is a seam, not v1.

**Relation to ego-proc's lifecycle: none, deliberately.** ego-proc's
`LifecycleStatus`/`ControlSignal` govern actors *inside* one process — the
connector and service actors of §9. The OS process's own lifecycle belongs to
whatever started it. Where that vocabulary does surface is in `ps`/`status`
output: one `ActorHealth` shape covering both populations, service actors and
guest agents, which is what §9 asks for. The CLI never grows a verb that
commands a guest agent, because commanding guest agents is a program's job.

## 13b. SSH over WSS (settled 2026-08-27)

Discofetch fetchpoints sit behind whatever NAT topology an endpoint happens
to have. The connectivity ladder, in order of preference: a direct path
where hole-punching works (STUN/WebRTC — most networks); and where it does
not (CGNAT, address-and-port-dependent filtering), the carrier that always
works is an **outbound WSS connection to something reachable**, with
ego-transport arranging the peer-to-peer leg where it can. SSH does not
care what carries it — the protocol is end-to-end over any reliable byte
stream — so DRT's obligation is **carrier flexibility, not a new
protocol**.

Two shapes, one principle (the bridge never looks inside):

- **`drt tunnel <wss-url>`** — bridge this process's stdio to the WSS
  connection: the OpenSSH `ProxyCommand` contract. This is what buys
  "works like normal SSH" without reimplementing any of it: `ssh -o
  ProxyCommand="drt tunnel wss://gate/fp" user@fp`, and rsync/sftp/`-L`/
  `-R`/agent forwarding are all the real ssh client's, inherited. Host-key
  verification and auth stay end-to-end between ssh and sshd; a gateway
  relaying the WSS leg can drop the connection but reads ciphertext. TLS
  on the `wss://` leg is belt over braces — middleboxes see ordinary
  HTTPS; it is not load-bearing for secrecy.
- **`drt tunnel --listen <addr> --to <host:port>`** — the other half:
  accept WebSockets, bridge each to a TCP target, in front of any sshd
  (today a stock one; later `drt start`'s own control endpoint).

**The seam this names upstream (ego-transport):** in-process composition —
the `host:ssh/exec` connector dialing *via* wss or webrtc, and the browser
tier doing SSH at all — wants `SshClientConnection` to accept an
already-open stream. russh has `connect_stream`; ego-transport's `connect`
dials TCP itself today, and its native WebSocket `recv` truncates a message
larger than the caller's buffer rather than buffering the tail. Both are
upstream asks, filed; when they land, the connector's scope grows a `via`
and the bridge loses no code, because the ProxyCommand shape stays useful
to the system ssh client forever.

## 13. Open questions (deliberately)

Final naming (DRT vs DRE); sql connector's verb surface
(`sql/query` stays as an ergonomic verb over fs-scope per Capabilities.md §4);
hibernated-instance usage accessor (v1: show hibernated + budget + cached
size, no usage figure — the agreed stub, not an oversight); whether
`drt-caps`/ForeignActor migrate into the ego family once a second consumer
exists.
