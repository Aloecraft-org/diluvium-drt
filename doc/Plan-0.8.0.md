# 0.8.0: SSH in a browser, and the core it runs on

**Status:** the plan of record for the release after `0.7.0`. Written
2026-09-24 from a planning session with the owner; the decisions in §0.1
are the owner's, the measurements are this session's, and each one names
how it was taken so it can be re-taken rather than trusted. Nothing in
§3 or §4 is built yet except where §7 says so.

The headline is **SSH from a stock browser to a fetchpoint behind NAT**,
end to end: the SSH session runs in the page, so neither the relay nor
Discofetch ever holds plaintext. Everything else here either serves that
or rides along because it is small and ready.

## 0. What ships

| § | what | why it is here |
|---|---|---|
| §1 | **diluvium 0.17.1** (dv ABI 2) with `numeric` on in `full` and `web` | the core has moved two minors; the bump is proven locally and small |
| §2 | **SSH in the browser, over the relay** | the headline; works today's relay and device unchanged |
| §3 | **The WebRTC host** (str0m, in `full`), at least to the browser client's M0 | the direct path; relay stays the fallback |
| §4 | **Stock WireGuard interop in CI** | nothing tests DRT's WireGuard against anyone else's |
| §5 | Deferred to 0.9: pool depth, HTTP ingress on the relay, the key as a WebSocket subprotocol, Wisp over the relay | each edits frozen code (`doc/Plan-2026-09.md` §0.2) |

### 0.1 Decisions, by the owner, 2026-09-24

- **Browser SSH is the point of the release.** The native side stays stock
  `ssh`/`rsync`/`sftp` with `ProxyCommand="drt tunnel …"`; there is no
  native `drt ssh` verb, because there is nothing for one to add.
- **Browsers get no TURN.** When there is no direct path, the fallback is
  Discofetch plus `drt tunnel`, i.e. the relay.
- **WebRTC goes into `full` now**, on the library this session judged best
  (str0m, §3.1), and moves out to a plugin once plugins land. This is the
  owner's waiver of `doc/Plan-2026-09.md` §0.2's 1 MB rule for this
  dependency, at the measured +2.78 MB.
- **`numeric` is on** in the profiles that carry the core's numeric tier.
- **RSA is in** the browser SSH client.
- **Auth is a key generated in the page, or a password.** FIDO/WebAuthn
  is the goal and comes after the Discofetch launch.
- **The SSH page is one static HTML file** for now, embedded in other
  places later.
- **The browser access client** (Scramjet, AGPL) is built in another
  session. DRT owns the wire it speaks to the host: `doc/BrowserAccess.md`
  and its test vectors.

---

## 1. The core: diluvium 0.17.1, dv ABI 2, `numeric`

DRT 0.7.0 embeds diluvium `v0.15.1` (`7f952d8`, ABI 1). `v0.17.1`
(`d8497b0`) is ABI 2; its tree is 0.16.0's, which is where the ABI moved.
Diluvium's `main` was exactly `v0.17.1` on 2026-09-24, and its changelog
says the ABI 3 work reaches `main` when 0.18.0 ships.

**Measured on a trial bump** (`cargo update -p diluvium --precise d8497b0`,
since reverted): it builds; the workspace tests pass except the two guards
built to catch exactly this (`version_first` in
`crates/drt-swarm/tests/diluvium_engine.rs` asserting ABI 1, and
`the_hard_coded_core_facts_agree_with_the_changelog`); the examples gate
on `--features full` is 24 ok, 1 failed (`00-install-methods`, whose
`expected.txt` says `dv_abi: 1`), 2 skipped (network, privilege).

The work:

1. **Pin the tag**: `diluvium = { git = …, tag = "v0.17.1" }` in the
   workspace `Cargo.toml`. It follows the default branch unpinned today,
   and once 0.18.0 lands a routine `cargo update` would pull ABI 3.
2. **`DILUVIUM_VERSION` to `"0.17.1"`** (`crates/drt/src/cli.rs:129`).
   Found by the trial: the binary would otherwise report
   `diluvium_version: 0.15.1` over a 0.17.1 core. 0.17.1 has no
   `dv_version()`, so this stays hard-coded, and the changelog test is
   still what keeps it honest.
3. **`features` read from the core.** `diluvium::library_features()`
   answers `dv_features()`; the four `CORE_FEATURES_*` tables and their
   `TODO(A0)`s go. Measured answers: `regex, json, msgpack, snapshot`,
   plus `numeric` with the feature on.
4. **`numeric` on** in `full` and `web`, through the `diluvium` crate's
   `numeric` feature. Measured: +75 KB stripped on a minimal native
   embedder (858 KB → 933 KB). The `web` cost is not measured yet and is
   measured before this lands, on the wasm artifact itself.
5. **Ignore `dv_build()`.** It returns a hard-coded 13 and upstream lists
   it as a known issue.
6. The two guard tests, `00-install-methods/expected.txt`,
   `doc/Release.md` §"The dv ABI rule" (the number moves; the rule does
   not), and the changelog entry.
7. Optional, and 0.8.1 if it is not ready: the `TODO(A0)` in
   `drt_hostcall::to_wire`, handing a connector's column to
   `dv_array_adopt` instead of copying it.

**Upgrade notes for the changelog** (from diluvium's own 0.16.0 entry):
snapshots taken under ABI 1 do not restore under ABI 2, so a deployment
holding hibernated instances drains or replays them first; `tostring` of a
table, function or coroutine prints `table: #N`, not an address; `pairs`
over object keys visits them in creation order. That is a guest-visible
change and a snapshot break, which is why this is 0.8.0 and not 0.7.1.

## 2. SSH in the browser, over the relay

```text
browser page (russh in wasm) ──wss /s/<label>──┐
                                               ├─► relay (unchanged) ◄── device: drt tunnel --park … --to 127.0.0.1:22 ──► stock sshd
ssh -o ProxyCommand="drt tunnel wss://…/s/…" ──┘
```

**One parked device serves both kinds of caller, and neither the device
nor the relay changes.** The page sends exactly the bytes a `ProxyCommand`
does. The relay already accepts a browser: binary frames only
(`crates/drt/src/relay.rs:436`), no subprotocol or Origin requirement, and
`?k=` is the one way a browser can present the key, since it cannot set
`Authorization` on a WebSocket.

### 2.1 The client

russh 0.63.1 (already in the lockfile) on its `ring` backend, compiled to
`wasm32-unknown-unknown`, driven over a WebSocket byte stream, with
xterm.js in raw mode. It lives in this repository as an Apache-2.0
module, so the AGPL browser access bundle can import it and not the
other way round.

**Measured** (a probe that really performs kex, password and key auth,
pty, shell, data and resize, so nothing is dead-code-eliminated; built
`opt-level = "z"`, fat LTO, then `wasm-opt -Oz`):

| part | raw | gzip |
|---|---|---|
| SSH client, Ed25519 | 920 KB | 338 KB |
| + RSA (in, per §0.1) | +85 KB | +32 KB |
| xterm.js 6.0 + css + fit addon | 497 KB | 124 KB |

**The single HTML file** carries the wasm as base64 of its gzip and
unpacks it with `DecompressionStream`: ~990 KB on disk with RSA, ~497 KB
over the wire when served gzipped. Plain base64 of the raw wasm is
~1.84 MB and ~620 KB. The page's own code and wasm-bindgen's glue are not
in those numbers.

### 2.2 What the page must do that `ssh` does for free

- **Host key trust**: trust on first use, kept in IndexedDB, and an
  optional pinned fingerprint in the link's fragment that must match.
- **Auth**: an Ed25519 key generated in the page (the user adds its public
  half to `authorized_keys` once), or a password. Keyboard-interactive is
  not built; nothing in 0.8.0 needs it.
- **Retry a 1013.** The device keeps one parked leg and re-parks after a
  claim (`tunnel.rs:685`), so two claims at once give one of them close
  1013 until the pool depth in §5 exists. The client retries with backoff.
- **Keepalive**, so the relay's idle close never ends a quiet session. An
  SSH keepalive, but on the page's timer: russh's own timers are
  `tokio::time`, which panics on `wasm32-unknown-unknown` (no clock std
  can reach), so the module runs none and exposes `keepalive()`.

**The link**: `https://<page>/ssh.html#url=<claim URL>&user=<name>&hostkey=SHA256:…`,
where the claim URL is the one `ProxyCommand` dials,
`wss://<relay>/s/<label>?k=<caller key>`. The fragment never reaches
whoever serves the page.

**The CI gate**: one job, one stock OpenSSH `sshd`, one relay, one parked
device; (a) stock `ssh` over `ProxyCommand`, (b) the page in Playwright
running a command and a resize, (c) both at once on one label, which is
the 1013 case.

## 3. The WebRTC host

The direct path: a browser and the DRT host connect over a WebRTC data
channel, signalled through Discofetch room presence, and the host carries
TCP streams to a locally configured scope using Wisp v1. The DRT side of
the spec is `drt-browser-access-spec.md`; the wire both sides implement
is **`doc/BrowserAccess.md`**, with test vectors both test suites load.
"Browser access" is a working name and is not in any public name here.

SSH joins it as one scope entry, `ssh://127.0.0.1:22`: the page opens a
Wisp stream instead of a relay leg, and nothing else about the SSH client
changes. The relay is the fallback when ICE fails.

### 3.1 The library: str0m 0.23, pure-Rust crypto

Both candidates were linked into a real `full` build answering an offer
with static ICE credentials on one muxed UDP socket and two negotiated
data channels (`--profile release-small`, x86_64 Linux gnu):

| | binary | added | added, gzip |
|---|---|---|---|
| `full` today | 8.78 MB | | |
| webrtc-rs 0.17 | 11.06 MB | +2.27 MB (+26%) | +0.99 MB |
| **str0m 0.23, `rust-crypto`** | 11.56 MB | **+2.78 MB (+32%)** | +1.26 MB |

str0m, although it is the larger:

- **The spec's socket model is its native model.** Every browser session
  answers to the same static host ufrag on one UDP socket.
  `Rtc::accepts` checks the local *and* the remote ufrag of a binding
  request, a binding response by transaction id, and DTLS by source
  address (`is-0.11.0/src/agent.rs:1089`), so routing is "offer the packet
  to each session until one accepts it". webrtc-rs's `UDPMuxDefault` keys
  on the local ufrag alone (`webrtc-ice-0.17.2/src/udp_mux/mod.rs:148`),
  so every session would collide on one connection, and its `UDPMux`
  trait is only ever asked for the local ufrag, so fixing that is a
  replacement mux.
- **No SDP on the host.** The direct API builds a session from the
  browser's record: `set_remote_ice_credentials`,
  `set_remote_fingerprint`, `add_remote_candidate`, `start_dtls(false)`,
  `start_sctp(false)`, `create_data_channel` with a negotiated id. The
  host never writes an answer, which is what the spec's "no answer round
  trip" means.
- **DRT owns the socket.** Server-reflexive candidates are only useful if
  they are the mapping of the socket the sessions use, so STUN gathering
  happens on that socket, in DRT, with `ego_transport::stun`'s sans-IO
  codec. It is also what lets the tests run without a network.
- When answering an `actpass` offer it takes `passive`
  (`src/change/sdp.rs:767`), which is the role the wire doc fixes.
- MIT OR Apache-2.0 throughout (str0m, `is`, `str0m-proto`,
  `str0m-rust-crypto`, `dimpl`), and pure Rust, so it builds on every
  release target with no cmake. It has no `ring` backend, which is most
  of why it costs 0.5 MB more: its crypto duplicates what `ring` already
  provides in the binary.
- The webrtc-rs in the lockfile is 0.17, pinned there by ego-transport,
  while upstream is at 0.21 (2026-09-19): adopting it would be adopting an
  old line.

### 3.2 Shape in DRT

A crate and a block, in the arrangement `turn` and `wireguard` already
have:

- **`crates/drt-rtc`**: the presence record codec, the Wisp v1 codec, and
  the host: one UDP socket, one `str0m::Rtc` per browser session, the
  Wisp server, and the TCP splice. Payload bytes never pass through Lua.
- **The `webrtc` block** (placeholder name, §6) served by `drt start` on
  its own runtime thread, with a report queue and a command queue:
  - reports `webrtc_record` (the host's own presence record, re-sent when
    a STUN answer changes its candidates), `webrtc_session`
    (`connected`, `closed`) and `webrtc_stream` (open, close, bytes each
    way, reason; never payload);
  - takes `{command = "open", peer, rtc}` and `{command = "close", peer}`.
- **Signaling is the program's**, over `rest`, exactly as WireGuard's
  rendezvous is (`doc/WireGuard.md` §4: "the exchange is a program's").
  The block never learns Discofetch's API, so the API can change without
  a DRT release, and the room credential lives where the program's
  secrets already do. This is the spec's M3 ("a dlua program on stock
  DRT") from the start.

### 3.3 Milestones

| | DRT | with |
|---|---|---|
| **M0** | record + Wisp codecs against the vectors; the host with a static scope from the block; a native str0m test client playing the browser (open, echo, out-of-scope refused, two concurrent sessions); a Chromium check that a locally built answer connects | the client's M0 (raw Wisp over the data channel, one page), against the mock signaling server |
| M1 | the policy hook: a CONNECT is asked of the program in the relay's `admit` shape before any TCP; audit events; the scope as a capability grant, so a buggy hook still cannot reach past it | client M1 (Scramjet) |
| M2 | limits, a node per session (the socket connector's `vital` / `transfer` handles), presence refresh, the spec's six tests | client M2 |
| M3 | SSH as a scope entry; the three-way CI job from §2.2 gains a fourth leg, the page over WebRTC | |
| M4 | the signaling program packaged for dollup | Discofetch service type |

**0.8.0 needs M0.** M1 onward ship when they are ready, in 0.8.x.

### 3.4 Things the spec leaves to be said

- **The scope must be checked after resolution too.** Scope matches a
  hostname string and the host resolves it afterwards, so a listed name
  that resolves to loopback, link-local or cloud metadata would get
  through. Unlisted special-purpose addresses are refused on the resolved
  address, as `connectors.rest` already does for the same reason.
- **A new capability, not a wider socket connector.** The socket
  connector deliberately has no `connect`, and a shipped connector's
  behaviour is not changed (§0.2 of the September plan). Outbound connect
  on a guest's behalf is its own grant.
- **Discofetch can intercept a session.** The fingerprints travel in
  presence, so whoever controls signaling could swap them; that is
  WebRTC's usual trust model. SSH still verifies its host key end to end,
  but a plain `http://` scope entry has nothing above it.
- **Host candidates publish the host's LAN addresses to the room.** A
  block setting publishes only server-reflexive candidates; the cost is
  that two machines on one LAN then connect through their router's
  public address, which only works where the router hairpins.

## 4. Stock WireGuard in CI

A Linux job with `wireguard-tools` and `wireguard-go` (or the kernel
module on a runner with `CAP_NET_ADMIN`) handshakes a stock peer against
DRT's userspace mode and carries traffic both ways. Tests only; no edit
under the WireGuard code. If it finds an incompatibility, that is the
release's first fix.

## 5. Deferred to 0.9, and why

Each of these edits code `doc/Plan-2026-09.md` §0.2 froze:

- **Pool depth on the device** (N parked legs), which ends the 1013 retry
  and is what makes serving a website through a fetchpoint reliable.
- **Hostname-routed HTTP ingress on the relay**, SNI passthrough so TLS
  stays end to end, and a way to admit anonymous visitors per label.
- **The key as a WebSocket subprotocol**, so a browser's key is not in the
  relay's request line.
- **Wisp over the relay**, which would give browser access the same
  fallback SSH has.
- **WebRTC as a tunnel carrier** (`webrtc://` in `drt tunnel`), with
  authenticated signaling rooms.

## 6. Open questions

- The `webrtc` block's name and its event names are placeholders until
  the owner confirms them; nothing public is named "browser access".
- Discofetch's real join and presence shapes, and whether presence is
  pushed or polled. M0 uses the mock in `doc/BrowserAccess.md` §7.
- Whether the SSH page's direct path imports the browser access client's
  connection module or carries its own (the answer decides whether the
  single HTML file stays single).
- Multiple services on one host socket, or one socket per service.
- Session caps: the host's `max_sessions`, Discofetch seats, or both.

## 7. Status

- 2026-09-24: this plan; `doc/BrowserAccess.md` and its vectors
  (`crates/drt-rtc/vectors/browser-access-v1.json`).
- 2026-09-24, **M0 (DRT side) done**, on `claude/drt-dlua-ssh-tunnel-cdflop`:
  - `crates/drt-rtc`: record and Wisp codecs against the vectors; the host
    on str0m with a static scope; `tests/host.rs`, a native client playing
    the browser: a scoped round trip, refusals before any connection
    (out of scope, a name for a literal, UDP, stream 0), two sessions on
    one socket, a session close closing its TCP socket, bad and duplicate
    records refused by name, and a 4 MiB download intact through str0m's
    128 KiB buffer.
  - The `webrtc` block in `drt start` (`crates/drt/src/webrtc.rs`), in
    `full` and in CI's lone-feature matrix; `identity_file` resolves
    against the config. The loader corpus snapshots each gained exactly
    `webrtc: None`.
  - **Chromium accepts a locally built answer** from the host's static
    record: `browser-check/check.mjs`, three sessions at once through one
    host socket and one host ufrag, each echoing through a Wisp stream,
    each with a record carrying no candidates. `check.mjs --drt` passes the
    same through `drt start`, `host.dlua` signaling over `rest` to
    `mock.mjs`.
  - Not proven: anything across a NAT (everything ran on one machine),
    and server-reflexive gathering against a real STUN server (the code is
    there; no test reaches one). Both need the browser client and a
    Discofetch room, which is the next pairing.
- 2026-09-24, **§1 done**: the pin names `tag = "v0.17.1"`; `numeric` is a
  `drt` feature in `full` and `web`, through `drt-swarm`'s; `buildinfo`'s
  `features` is `dv_features()`; `DILUVIUM_VERSION` is `0.17.1`; the
  config's numeric bounds reach the core and the fast-tier flag is the
  core's. The tree is `0.8.0` with an unreleased changelog entry, so dev
  builds from here are `v0.8.0-dev.N`. Measured: `numeric` costs the
  browser module +123.5 KB (3,284,888 → 3,408,417 bytes; +26.8 KB
  gzipped), `release-small`, wasi-sdk 27. Verified: both test suites, the
  native gate on `full`, the Chromium gate (12 ok, REPL parity included)
  and the wasmtime gate on `wasi` (10 ok). Item 7 (`dv_array_adopt` in
  `to_wire`) is not done and moves to 0.8.1 unless it is ready first.
- 2026-09-24, **PR #37's CI overflowed a 2 MiB test thread** in the 4 MiB
  download test. str0m's `Rtc::do_poll_output` recurses once per SCTP
  packet it hands to DTLS (24.7 KB a frame in debug), and the host ran on
  its caller's stack. Fixed in `f81dc97`: the host runs on its own thread
  with a 16 MiB stack, and the test drives its client from 512 KiB so it
  keeps proving the depth never lands on the caller. Worth an upstream
  issue: the recursion should be a loop.
- 2026-09-24, `sockets.rs`'s pin guard: acceptance 13 (a hibernated
  service keeps its socket and queue handles across a whole-instance
  snapshot, §3.5 of the 0.7.0 plan) re-proved on `d8497b0` and
  `DILUVIUM_VERIFIED` moved to it.
- Decisions made while building, per `doc/Plan-2026-09.md` §0.2:
  - `numeric.max_elements = 0` is withheld from the core rather than
    passed as dv's "no limit" 0; a known issue in the changelog, and an
    upstream ask for a sentinel.
  - `features` is reported in the core's own order, not sorted: `dv.h`
    fixes that order so two builds' strings compare directly.
  - The M0 rehearsal deployment lives in `crates/drt-rtc/browser-check/`,
    not `examples/`: `examples/` is a declared human surface, and a config
    there must join the loader corpus. It becomes an example when the
    signaling program is packaged (M4).
  - The spec's 1 MiB high-water mark is 96 KiB with a 32 KiB low-water
    mark, because str0m refuses writes past 128 KiB buffered.
  - UDP `CONNECT` is refused with `0x48`, the one departure from Wisp v1.
  - A browser must cap its wait for ICE gathering; the check uses 2 s.
- 2026-09-24, **§2 built, not yet in CI**:
  - `crates/drt-ssh-web`: russh 0.63.1 (`ring`, `rsa`) for
    `wasm32-unknown-unknown` behind `Ssh.connect / authPassword / authKey /
    shell / write / resize / keepalive / close` and `generateKey`. The
    host key is decided between key exchange and authentication: a pin
    that does not match fails the connect before anything authenticates.
    Every dependency is wasm-only, so native russh's features are
    unchanged.
  - `crates/drt-ssh-web/page/`: `ssh.html`, one file with xterm.js 5.5 and
    the module inlined (gzip, then base64). TOFU host keys and the page's
    own key live in IndexedDB; a close 1013 is retried with backoff.
    `script/drt-ssh-page.sh` builds it. Measured: 861,091 bytes on disk,
    472,562 gzipped; the module inside is 956,993 bytes.
  - `page/e2e.mjs` is the §2.2 gate: stock OpenSSH 9.6 sshd, `drt start`
    as the relay, `drt tunnel --park`, then stock `ssh` over
    `ProxyCommand`, the page signing in with a pinned key, a resize, a
    second page and stock `ssh` at once beside the open session, a wrong
    pin refused with nothing authenticated, TOFU shown and accepted, and
    `exit 3` reaching the page. **All 7 passed**, with the harness and
    sshd running as root.
  - Found since that pass, and fixed but not yet re-run through the page:
    the connect's failure paths dropped the socket's Rust handlers while
    the socket could still fire, so Chromium threw "closure invoked after
    being dropped" on a refused pin. The socket and its handlers are now
    one value that detaches them on drop, and the e2e gained an eighth
    check, that no page raised an uncaught error.
  - **The gate needs a root sshd.** An unprivileged OpenSSH cannot give a
    pty to the `tty` group or write login records, and closes every pty
    session; shown with stock `ssh -tt`, so it is sshd, not the page.
    `e2e.mjs` runs sshd through `sudo -n` when it is not root, with
    `UsePAM yes`, because a root sshd without PAM refuses a locked
    account and a fresh CI user's is locked. The CI job is not written.
- 2026-09-24, **browser access signaling moves to a socket** (the owner
  took it into 0.8.0):
  - `connectors/ws` is the `ws` connector, in `full`, under `rest`'s
    origin allowlist. It allows `wss://` only, with plain `ws://` to
    loopback alone, because the advertise token rides the upgrade.
  - `crates/drt-rtc/signal/host.dlua` holds the socket.
  - `webrtc.stun_refresh_s` defaults to the contract's 20 s, and
    `open`'s `peer` is capped at 64 bytes.
  - The contract's seven host tests pass in `crates/drt/tests/signal.rs`,
    against a `wss://` stub with the native client as the browser.
  - Not proven: the real API (not live yet), and a browser rather than
    the native client on this path.
  - Decisions:
    - `browser_access.signal` is `args.signal`, because args are flat.
    - `record` goes out as an object spliced from the host's own text, and
      comes in as an object or text. dlua's JSON turns `[]` into `{}`, so
      an object is rebuilt field by field.
    - `replaced` ends `drt start`.
    - Reconnect policy is the program's, not the connector's: the
      connector reports how a connection ended and never redials.
