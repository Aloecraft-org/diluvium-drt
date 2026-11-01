# The browser access wire, v1

**Normative.** This is the wire between a browser and a DRT host that
reach each other directly over WebRTC: what each side publishes in room
presence, how the browser builds a session from the host's record without
an answer round trip, which data channels exist, and the Wisp v1 profile
the host serves on one of them. DRT owns it, as it owns the relay's URLs
(`doc/Relay.md`) and the hostcall encoding (`doc/Hostcall.md`); the
browser client and Discofetch consume it.

"Browser access" is a working name. Nothing public is named after it
until the owner confirms one, and every block, event and path name below
that is not a WebRTC or Wisp term is a placeholder for that reason.

The test vectors are `crates/drt-rtc/vectors/browser-access-v1.json`.
Both test suites load that file; the browser client vendors it by commit.
`crates/drt-rtc/tests/vectors.rs` fails when the file and this
implementation disagree, so the file is never edited by hand: change the
implementation, regenerate it (`DRT_WRITE_VECTORS=1 cargo test -p drt-rtc
--test vectors`), and review the diff.

**Status (2026-09-24):** draft v1, written for the M0 pairing of the host
(`doc/Plan-0.8.0.md` §3) with the browser client's M0. §7.1 is the host's
side of Discofetch's signaling socket; §7.2 is M0's mock, kept for
`check.mjs`. §3.4, direct mode, needs no signaling at all.

**What is verified, and how.** Everything below the record codec is
exercised by `crates/drt-rtc/tests/host.rs`, a native client doing what a
browser does over loopback. The two claims only a browser can settle --
that Chromium accepts an answer it built from the host's static record, and
that several sessions share one host socket and one host ufrag -- are
checked by `crates/drt-rtc/browser-check/check.mjs` against headless
Chromium 1194 (Playwright 1.56.1): three sessions at once, each publishing
a record with **no candidates at all**, each connecting and echoing
through a Wisp stream. `check.mjs --drt` runs the same against `drt start`
with the §8 block and a program signaling through the §7.2 mock.
`crates/drt/tests/signal.rs` runs `drt start` with §7.1's program against
a `wss://` stub of the API.

**The browser half ships as a library** (§9): `drt_browser_access.js`,
built from `crates/drt-rtc/client/` and attached to every release and dev
build. `check.mjs` drives that file, not a copy of its logic.

## 1. The shape

```text
browser ──join, publish record, read records──► room presence ◄──publish record, read records── DRT host
browser ◄════ ICE / DTLS / SCTP, two data channels, direct ════► DRT host ──TCP──► scoped targets
```

1. The host publishes one **record** (§2) in presence. It is static: the
   same ICE credentials, the same DTLS certificate and the same socket
   for every session, so it changes only when its candidates do.
2. A browser creates its peer connection and data channels (§4), makes an
   offer, applies it locally, waits for candidate gathering to complete,
   and publishes its own record.
3. The browser builds the host's answer locally from the host's record
   (§3.2) and applies it. ICE starts from the browser's side at once.
4. The host reads the browser's record from presence and creates a
   session from it (§3.3). ICE completes, then DTLS, then SCTP.
5. The host sends `hello` on `control` (§5). The browser opens streams on
   `wisp` (§6).

The host never sends anything through presence but its own record, and
nothing directed at one browser ever passes through Discofetch.

## 2. The record

One JSON object, carried in two forms, and **a reader accepts both**:

- **An object.** The real Discofetch API (§7.1) sends it this way, in a
  `peer`'s `record` and in its `rtc` presence field, and the host sends
  its own `record` this way.
- **A string** holding the object's JSON text. The M0 mock (§7.2) and
  the test vectors use it, and a reader must still take it.

Whichever form arrives, the reader checks the same rules below. The
budget is on the object's compact JSON text: `drt`'s program rebuilds
that text field by field, never by re-encoding the whole object (§7.1).

```json
{"v":1,"u":"Xk3fQ9aBc2Dd7eFg","p":"8bqS0lK1vT6YpR2eWm4nHc","f":"EBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8=","c":["candidate:1 1 udp 2130706431 192.168.1.20 50212 typ host","candidate:2 1 udp 1694498815 203.0.113.7 50212 typ srflx raddr 0.0.0.0 rport 0"]}
```

| key | type | rule |
|---|---|---|
| `v` | integer | `1`. A reader refuses any other value. |
| `u` | string | ICE ufrag: 4 to 32 characters, each `A-Z a-z 0-9 + /` (RFC 8839 `ice-char`). |
| `p` | string | ICE password: 22 to 64 `ice-char`s. |
| `f` | string | SHA-256 of the peer's DTLS certificate: standard base64 (RFC 4648 §4), padded, so exactly 44 characters decoding to 32 bytes. |
| `c` | array of strings | 0 to 8 candidate lines (§2.1). |

- **At most 512 bytes**, measured as the UTF-8 length of the record's
  JSON text: the string, or an object's compact encoding.
- **Keys may come in any order**, and a writer emits no whitespace. The
  host writes `v, u, p, f, c` in that order; nothing may depend on it.
- **Unknown keys are ignored.** A change a v1 reader cannot ignore is
  `v: 2`, not a new key.
- A record that breaks a rule above is refused whole. A candidate line
  that starts `candidate:` but cannot be used is skipped, not refused
  (§2.1). The budget and the 8-line count are the record's as received.

### 2.1 Candidate lines

Exactly the string `RTCIceCandidate.candidate` yields: it starts with
`candidate:`, has no `a=` prefix and no line ending, and follows RFC 8839
§5.1.

- **UDP only.** TCP candidates are omitted by the writer and skipped by
  the reader.
- **`typ host`, `srflx` or `prflx`.** No `relay`: there is no TURN in v1.
- **No trailing extensions.** A browser strips everything after
  `typ <type>` and, when present, `raddr <addr> rport <port>`:
  `generation`, `ufrag`, `network-id`, `network-cost`. They are what
  would push a record past 512 bytes, and nothing here reads them.
- **The browser omits `.local` (mDNS) candidates.** The host could not
  resolve them and does not need them: the browser is controlling, its
  checks reach the host, and the host learns the browser's address from
  them as a peer-reflexive candidate.
- **What a reader keeps.** A line is kept when it reads as RFC 8839 §5.1
  through `typ <type>` (numeric component, priority and port, the port at
  most 65535), its transport is `udp`, its type is `host`, `srflx` or
  `prflx`, and its address does not end `.local`, all compared without
  case. Every other line is dropped on the way in, so no answer a browser
  builds (§3.2) and no session a host builds (§3.3) sees one. Trailing
  extensions do not make a line unusable. `usable_candidate` in
  `crates/drt-rtc/src/record.rs` and `isUsableCandidate` in the client
  library (§9) are this rule, held to the same cases, and the vectors
  carry records with lines a reader skips: their `decoded` and
  `answer_sdp` are what is left.
- **The host's `srflx` lines carry `raddr 0.0.0.0 rport 0`**, as
  browsers' do, so a host that publishes only its public candidates
  (`doc/Plan-0.8.0.md` §3.4) does not leak its LAN address through them.

### 2.2 Identity

A host generates its ufrag, password and certificate once and keeps them
(`identity_file`, §8). A browser's are its peer connection's own, fresh
per page load.

**A session is named by the browser's ufrag.** The host refuses a record
whose ufrag matches a session it already has; the client rejoins, which
gives it a fresh one.

## 3. Roles and session setup

| | browser | host |
|---|---|---|
| SDP | makes an offer, applies it; builds the answer (§3.2) | none: str0m's direct API (§3.3) |
| ICE | controlling, full ICE | controlled, full ICE (not ICE-lite: a host behind NAT must send checks to open its own mapping) |
| DTLS | client (`setup:active`, from the offer's `actpass`) | server (`setup:passive`), fixed certificate |
| SCTP | port 5000 | port 5000 |

### 3.1 The browser's offer

The offer is ordinary and needs no munging: `createDataChannel` twice
with `negotiated: true` (§4), `createOffer`, `setLocalDescription`, then
wait for `iceGatheringState === "complete"` and publish once. **Publish
after gathering completes rather than trickling**: presence is not a
message bus, and a record that changes under a host that has already read
it helps nobody.

**Cap the wait.** Gathering can stall short of `complete` -- an unreachable
STUN server does it, and so does the container this was verified in, where
Chromium's gathering never completed at all. The check publishes what it
has after 2 seconds. A record with no usable candidates still connects,
because the browser is controlling and the host learns its address from
its checks; what a thin record costs is a host behind NAT that cannot
open its own mapping towards the browser early.

### 3.2 The answer the browser builds

From the host's record and the offer's own `a=mid` value `<mid>`:

```text
v=0
o=- 0 2 IN IP4 127.0.0.1
s=-
t=0 0
a=group:BUNDLE <mid>
m=application 9 UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 0.0.0.0
a=mid:<mid>
a=ice-ufrag:<u>
a=ice-pwd:<p>
a=fingerprint:sha-256 <f as 32 upper-case hex pairs joined by ":">
a=setup:passive
a=sctp-port:5000
a=max-message-size:262144
a=candidate:<each c, after its "candidate:" prefix>
a=end-of-candidates
```

Lines end in CRLF. `answer_sdp` in `crates/drt-rtc/src/record.rs` is the
reference, and the vectors carry its output for each record, so a client
can compare byte for byte.

### 3.3 The session the host builds

From the browser's record: remote ICE credentials `u`/`p`, remote
fingerprint `f`, each usable candidate in `c`, `start_dtls(false)`,
`start_sctp(false)`, and the two channels of §4 created with their fixed
ids. The host never produces SDP.

**One UDP socket serves every session.** A packet belongs to the session
whose `Rtc::accepts` takes it: a binding request by the (host ufrag,
browser ufrag) pair in `USERNAME`, a binding response by transaction id,
DTLS and SCTP by source address. STUN answers to the host's own
candidate gathering (§8) are taken off the socket before any session
sees them, by transaction id.

**A session ends** when ICE reports it disconnected, when its browser's
presence expires (the program sends `close`), or when the host is asked to
close it. Ending it closes every Wisp stream it holds and every TCP
socket behind them.

### 3.4 Direct mode

A session with no signaling: the browser holds the host's record, got
however the host's owner hands it out (a link, a QR code, a config), and
nothing travels back. The host's record already says everything the
browser needs (§1: it is the same for every session). What the host lacks
is the browser's ICE credentials and certificate, and direct mode supplies
the first by rule and does without the second. It is off unless the
block's `direct` is on (§8); signaled sessions work beside it either way.

**The browser** makes an ordinary offer (§3.1's channels), replaces its
`a=ice-ufrag` and `a=ice-pwd` lines with one value it chose, applies that,
and builds the answer from the host's record as §3.2 says. The value is
32 random `ice-char`s (`DIRECT_UFRAG_LEN`), used as both ufrag and
password. It publishes nothing and need not wait for gathering: it is
controlling, and its checks go straight to the host's candidates.

**The host**, when a binding request arrives that no session accepts,
reads its `USERNAME`. When the host half is this host's ufrag and the
browser half is 22 to 32 `ice-char`s (`drt_rtc::host::DIRECT_UFRAG_LEN`),
it builds a session as §3.3 does with the browser's record taken to be
`{u: <browser half>, p: <browser half>, c: []}`, and keeps it only if that
session accepts the request: its `MESSAGE-INTEGRITY` must verify against
this host's password. Anything else is dropped unanswered, as an unknown
datagram always is. The session's peer is `direct:<browser ufrag>`, and
it is reported and ended as any other.

- **The record is the grant.** Integrity against the host's password is
  the only check, so whoever holds the record can open a session, up to
  `max_sessions`, to the block's scope and nothing else. A host with
  `direct` on hands its record only to whoever may reach that scope, and
  a new `identity_file` revokes every copy.
- **The browser's certificate is not checked.** Its fingerprint was
  never sent, so the host runs DTLS with fingerprint verification off.
  DTLS still encrypts, and the host's own certificate is still checked by
  the browser against `f`; what goes unproven is which browser it is,
  which is the job of whatever runs over the stream. SSH does it: the
  server's key is pinned, and the user authenticates.
- **Browser ufrags never collide with it.** Chromium's are 4 characters
  and Firefox's 8, so a signaled browser's checks never read as direct
  mode's, even before its `open` arrives.

Verified by `crates/drt-rtc/tests/host.rs` (a session from the record
alone; nothing without `direct`, with a wrong password, or with a short
ufrag) and in Chromium by `crates/drt-ssh-web/page/e2e.mjs`, where the
shipped SSH page reaches a stock sshd from a link carrying only the
record.

## 4. Data channels

| label | id | payload | |
|---|---|---|---|
| `control` | 0 | text: one JSON object per message | §5 |
| `wisp` | 1 | binary: one Wisp packet per message | §6 |

Both are `negotiated: true` with the fixed id, ordered and reliable. No
in-band open (DCEP) is sent or needed; both sides create both channels.
**Every message on either channel is at most 16 KiB (16384 bytes).**

## 5. `control`

The host sends one message after the channel opens:

```json
{
  "v": 1,
  "t": "hello",
  "service": "Home Assistant",
  "default": {"scheme": "http", "host": "127.0.0.1", "port": 8123},
  "scope": [
    {"scheme": "http", "host": "127.0.0.1", "port": 8123, "label": "Home Assistant"},
    {"scheme": "http", "host": "192.168.1.20", "port": 8096, "label": "Jellyfin"}
  ],
  "limits": {"max_streams": 64}
}
```

- `t` names the message. `hello` is the first, and the only one a v1
  reader must know; the others below are additive. A reader ignores a
  message whose `t` it does not know.
- `scheme` tells the browser whether to speak TLS itself (`https`), plain
  bytes (`http`), or SSH (`ssh`). **The host enforces host and port
  only.**
- `label` on a scope entry is optional, and the M0 host sends none: its
  block names entries as bare `scheme://host[:port]` strings.
- The scope travels over the data channel and nowhere else. It is the
  operator's network layout, and it never reaches Discofetch.
- `default` is omitted when the host names none; `service` is the block's
  label.
- **`scope` may be empty and `default` absent** (`doc/P2P.md` §7.2). A
  host shows its entries when its block's `hello_scope` is on, which the
  `webrtc` block's is by default; `drt p2p` hides them, sending `scope` as
  `[]` and naming the services only. `scope` is always an array, as every
  v1 client reads it. A reader that finds it empty asks for a named
  service, or for no target at all (§6).
- **`forwarding`**: `true` when the host is a relay (`doc/P2P.md` §4.4),
  which calls a destination the caller names and joins the two sessions.
- **`caps`**: capability names a program or REPL behind the host may hold
  (`doc/P2P.md` §7.2), such as `host:time/*`. Omitted when there are none.
  Descriptive: what a session gets is the host's decision at open time,
  and the host says so with **`granted`**:
  `{"t":"granted","stream":N,"caps":[…]}` on `control`, once the service
  behind stream N knows, which for the REPL behind a key is after the key
  signed in. A stream a service never reports on gets no message.
- **The caller's messages.** `{"t":"resize","stream":N,"cols":C,"rows":R}`
  reports the terminal size of one of the caller's streams, for a service
  that is a terminal (`doc/P2P.md` §5.2). `{"t":"call","to":"<peer>"}`
  names a relay's destination (§4.1); the relay answers
  `{"t":"called","hello":…}` or `{"t":"failed","why":…}`.
- **`scope` is not a page's boundary.** The host enforces host and port;
  a page that fences its own navigation on `scope` is trusting a hint,
  and the hint may be empty. A page's boundary is what its session was
  opened for: a page opened for the service `ssh` routes to that stream
  and nothing else, and one opened for the empty target routes to that
  and nothing else. It does not attempt a destination it was not opened
  for; if it does, the host's `CLOSE 0x48` is what holds the line, and
  the page shows the refusal. When `scope` is present it is the list the
  operator chose to show, fit for a picker, not a rule the page relies
  on.

## 6. `wisp`: the host's Wisp v1 profile

Wisp v1 as written in `MercuryWorkshop/wisp-protocol` `protocol.md`
(version 1.2), with the data channel in place of the WebSocket. What the
host does, and where it departs:

- **On channel open** the host sends `CONTINUE` on stream 0 with the
  initial per-stream buffer, **128** packets. Stream 0 is also the
  version marker: a v2 client that sees `CONTINUE` first knows it is
  talking to v1.
- **`CONNECT`** (`0x01`): stream type `u8`, port `u16` LE, hostname
  UTF-8. The host validates, then checks scope, then connects:
  - **The scope check**: allowed only if the hostname equals a scope
    entry's host string, compared case-insensitively, and the port
    equals that entry's port. An IP literal matches only by exact
    string. A refusal is `CLOSE 0x48`, and **no connection is attempted**.
  - **After resolution**, loopback, link-local, unspecified, multicast,
    broadcast and cloud-metadata addresses are refused with `0x48` unless
    the entry's host string is itself that address, or the entry is
    `localhost` and the address is loopback. A name in scope that resolves
    into one of them is exactly what scope is supposed to stop.
  - **UDP (`0x02`) is refused with `0x48`.** Wisp v1 makes UDP
    mandatory; this profile does not carry it. This is the one
    departure from the protocol.
  - **An empty hostname asks for whatever the host forwards to**
    (`doc/P2P.md` §5.1): with port 0, that target itself; with a port, the
    forward's host at that port. A host that forwards nothing (the
    `webrtc` block's shape) refuses it with `0x41`, as it always did.
  - An unknown stream type, a non-UTF-8 hostname, port 0 with a hostname
    that is not a service name (§10.3), or a stream id already open:
    `CLOSE 0x41`.
  - A name that does not resolve: `0x42`. No answer within the connect
    timeout: `0x43`. Refused: `0x44`. Any other failure: `0x03`.
  - Past `max_streams` open streams on the session: `0x49`.
- **`DATA`** (`0x02`) from the browser is queued per stream, FIFO, and
  written to the TCP socket in order. The host sends `CONTINUE` with the
  space left (128 minus what is queued) whenever it has written half a
  buffer's worth since the last one, and always before the browser's
  credit can reach zero. `DATA` for a stream that is not open is dropped.
- **`DATA` to the browser**: at most 16379 payload bytes per packet (16
  KiB less the 5-byte header). The host stops reading a session's TCP
  sockets while its channels hold more than **96 KiB**, or while it holds
  any packet the WebRTC stack refused, and resumes below **32 KiB**. The
  spec's draft said 1 MiB; str0m buffers at most 128 KiB across a
  session's channels and refuses a write past that, so the mark sits
  below it. A 4 MiB download arrives whole and in order through it
  (`a_large_download_arrives_whole_and_in_order`).
- **`CLOSE`** (`0x04`) from the browser closes the stream and its socket.
  The host sends `0x02` when the target closes cleanly and `0x03` when it
  fails. A `CLOSE` for a stream that is not open is ignored.
- **`END`** (`0x05`, this profile's one extension): no payload; the
  sender will write no more on the stream and still reads it, TCP's
  half-close, so `printf x | drt p2p <peer>` gets its answer. A peer that
  does not know the type ignores it (below), so each side sends `END`
  only to a peer that said it understands: the host says `half_close:
  true` in `hello`, and a caller says `{"t":"features","half_close":true}`
  on `control` once the channels are open. From the browser, `END` shuts
  the target's write side and reads go on; from the host, `END` is the
  target's end of stream to a peer that announced it (else `CLOSE 0x02`
  as before). Both halves ended, the host sends `CLOSE 0x02`. In the
  library, closing a stream's `writable` is the half-close; `close()` is
  the whole.
- **Unknown packet types are ignored**, and so is a packet shorter than
  its 5-byte header.

| reason | when the host sends it |
|---|---|
| `0x02` | the target closed the connection |
| `0x03` | the target connection failed after it was established |
| `0x41` | a malformed `CONNECT`, or a stream id already in use |
| `0x42` | the hostname does not resolve |
| `0x43` | no answer within the connect timeout |
| `0x44` | the target refused the connection |
| `0x47` | a stream idle past the idle timeout |
| `0x48` | out of scope, a special-purpose address, or UDP |
| `0x49` | the session's stream cap is reached |

## 7. Signaling

### 7.1 The host's socket to the Discofetch API

The host holds one WebSocket open to the Discofetch API, and the API is
the far end of it. The contract, browser side included, is Discofetch's
(`doc/BROWSER-ACCESS-SIGNALING.md` in the discofetch repository, as of
36ad148). This section is what DRT's side does
with it. The record (§2) is unchanged; only how it travels changed. This
is not the relay's park door (`/s`), which is separate and unchanged.

- **The program**: `"entry": "stdlib:browser-access"`, carried in the
  binary (source: `crates/drt/src/stdlib/browser_access.dlua`), under
  `drt start` with the §8 block and the `ws` connector. Its caps are
  `host:ws/*` and `host:time/monotonic`. It is the only Discofetch-
  specific code; the binary knows nothing of the message types.
- **The config** it reads:
  - `args.signal` is the socket URL. It is the contract's
    `browser_access.signal`, flat because a deployment's args are flat,
    and it is never built in code.
  - `args.key`, optional, is the advertise token, sent as
    `Authorization: Bearer` on the upgrade.
  - `connectors.ws.scope` is the origin allowlist, in `rest`'s shape. A
    deployment may instead inject the token there, as an `authorization`
    entry in the allow entry's `headers`, which the program cannot read.
    It then omits `args.key`: with the key set, the program sends the
    header too and the connector refuses the connect, because a program
    may not set a header its scope injects.
- **Transport**: `wss://` only. The token rides the upgrade, so the `ws`
  connector refuses plain `ws://` anywhere but loopback: in the scope at
  boot, at connect, and again on the resolved address.
- **Frames**: text, one JSON object each. The host sends:
  - `record`, with the record as a JSON **object**, spliced unparsed from
    the host's own record text. It goes out on every connect and whenever
    the record changes (§8's `stun_refresh_s`).
  - `outcome`, `direct` or `failed`, exactly once per session.
  - `bye`, with `busy`, `failed` or `closed`, when the host ends a
    session. A session the API ended with its own `bye` gets none back.
- **The `record` in a `peer`** is taken as a JSON object or as the
  record's text. An object is rebuilt field by field, never re-encoded
  whole: dlua's JSON would turn an empty `c` into `{}`.
- **Sessions**: one per `peer` the API announced, and no other. ICE from
  a ufrag the API never announced finds no session in the host (tested).
  `session` is an opaque key of at most 64 bytes, refused past that by the
  program and again by the `webrtc` block.
- **The socket's life**:
  - `ws/connect` runs with `idle_ms = 60000`, so 60 s with no frame at
    all, pings included, closes the socket.
  - Reconnect backs off from 1 s, doubling to 30 s, plus up to 25%
    jitter. `ready` resets the backoff.
  - Sessions do not depend on the socket. Outcomes and byes wait for the
    next socket, up to 256 of them.
  - A close code in 4000–4099 is the API not wanting this socket back:
    4001, a newer host socket replaced it; 4003, the credential was
    refused. The program stops, `drt start` exits, and nothing dials
    again. Any other close, and 60 s of silence, is redialed with backoff.
    The `ws` connector reports the close code with the close, so none of
    this reads the API's JSON.
  - There is no `answer` message: the browser builds the answer from the
    host's record (§3.2).
- **Limits**: the API adopts §2's record and limits (512 bytes, 8
  candidates, `u` 4–32, `p` 22–64) and validates against
  `crates/drt-rtc/vectors/browser-access-v1.json`. No `v` bump.
- **Tested** in `crates/drt/tests/signal.rs`, the contract's seven host
  tests, against a `wss://` stub of the API with the native client as the
  browser.

### 7.2 M0's mock (superseded by §7.1)

**Not Discofetch's API.** A stand-in both sides point at until
Discofetch's `rtc` presence field and CORS exist. The browser client's
session builds it. It is here so the host's example program and the
client agree on one shape, and it is mapped onto the real API when that
is known. Every call is `POST` or `GET` because those are the two
`connectors.rest` has.

```text
POST /v1/rooms/{room}/join       {"credential": {"pin": "…"} | {"token": "…"} | {}}
                                 -> 200 {"peer_id", "session_token", "presence_ttl_s"}
POST /v1/rooms/{room}/presence   Authorization: Bearer <session_token>
                                 {"kind": "host" | "browser", "rtc": "<record>"}
                                 -> 204
GET  /v1/rooms/{room}/presence   Authorization: Bearer <session_token>
                                 -> 200 {"peers": [{"peer_id", "kind", "rtc", "expires_at"}]}
```

- A presence `POST` also refreshes its expiry. A peer that stops posting
  for `presence_ttl_s` disappears from the list.
- `crates/drt-rtc/browser-check/mock.mjs` is one implementation, with open
  CORS. It also serves a blank page at `/`, because Chromium will not let
  a page with no origin (`about:blank`) call a loopback address at all;
  that is an artifact of testing on one machine, not a rule.
- The host polls every second in M0. It must see a browser's record
  promptly: a host behind NAT opens its own mapping only when it starts
  sending checks towards the browser, and the browser gives up after
  about 15 seconds.
- Whether Discofetch pushes or is polled is open (`doc/Plan-0.8.0.md` §6).

## 8. The host's block (placeholder name `webrtc`)

```json
{
  "webrtc": {
    "bind": "0.0.0.0:0",
    "identity_file": "webrtc-identity.json",
    "stun": ["stun1.discofetch.link:3478", "stun2.discofetch.link:3478"],
    "publish_host_candidates": true,
    "service": "Home Assistant",
    "default": "http://127.0.0.1:8123",
    "scope": ["http://127.0.0.1:8123", "http://192.168.1.20:8096", "ssh://127.0.0.1:22"],
    "max_sessions": 12,
    "max_streams_per_session": 64,
    "idle_stream_timeout_s": 300,
    "connect_timeout_s": 10,
    "stun_refresh_s": 25,
    "direct": false,
    "queue": "webrtc",
    "reply_queue": "webrtc_cmd"
  }
}
```

- **`services`**, name to scope entry, names what a peer may open a
  stream to without an address (§10.3): `{"ssh": "ssh://127.0.0.1:22"}`.
  Each name must be one §10.3 allows and each entry must be in `scope`,
  or the host refuses to start. `hello` lists the names.
- **`direct`**, off by default, turns on §3.4: a caller holding the
  record makes a session with no signaling. The program still gets every
  report, `webrtc_session` included, with `direct:<ufrag>` as the peer.

- **`identity_file`** holds the ufrag, password and certificate, relative
  to the config file like `program`. Created `0600` on first start when
  missing, and read thereafter, so the record survives a restart. A file
  that exists and does not parse is refused, never replaced: a new
  identity would strand every room holding the old record.
- **`stun`**: server-reflexive candidates are gathered from these, on the
  session socket itself, because a mapping only means something for the
  socket it was measured on. **`stun_refresh_s`** re-asks them that often
  once they have answered. That keeps the socket's NAT mapping open
  between sessions; home routers drop an idle UDP mapping after 30 to
  120 s. A mapping that moved changes the record, which is reported again.
- **Reports on `queue`**, each a map with `event`:
  - `webrtc_record` `{rtc}`: the host's record, on start and whenever its
    candidates change. The program publishes it.
  - `webrtc_session` `{peer, state = "connected" | "closed", reason?}`.
  - `webrtc_stream` `{peer, stream, host, port, state = "open" | "closed",
    reason?, bytes_up, bytes_down}`: `up` is browser to target. A refused
    `CONNECT` is reported too, closed with its reason, which is the entry
    an audit most wants. Never payload.
- **Commands on `reply_queue`**: `{command = "open", peer, rtc}` makes a
  session from a browser's record; `{command = "close", peer}` ends one.
  A refused `open` is reported as `webrtc_session` `closed` with the
  reason. `{command = "call", peer, rtc}` makes a session by calling: the
  program, told by its own signalling to call another peer
  (`doc/DRT-Signalling.md` §6.2), makes the caller's request with the
  record it holds from `webrtc_record` and hands the answerer's reply here;
  the host is the calling side of that session and serves on it as on any
  other, reported the same way. Whether to follow such an instruction is the
  program's to decide, as every other part of its signalling is; `peer` is
  a name of its choosing for the session.
- **`crates/drt/src/stdlib/browser_access.dlua`** does §7.1's signaling;
  `crates/drt/tests/signal.rs` shows a working config for it. M0's
  `crates/drt-rtc/browser-check/host.json` and its `host.dlua` still do
  §7.2's over `rest`.

## 9. The client library

`crates/drt-rtc/client/drt_browser_access.js`, with its types in
`drt_browser_access.d.ts`, is §2 to §6 on the browser's side: one ES
module, no dependencies, no build step. Every release and dev build
attaches both files. A client may use it or implement these sections
itself; the vectors hold either to the same bytes.

```js
import { offer, direct } from './drt_browser_access.js';

const pending = await offer({ iceServers });     // §3.1: gathers, capped at 2 s
// pending.record goes to the host through signaling (§7.1).
const session = await pending.accept(hostRecord); // §3.2; object or text (§2)
session.hello;                                     // §5
const s = session.connect('127.0.0.1', 8123);      // §6: must be in hello.scope
// s.readable / s.writable: Web Streams of bytes. s.closed rejects with a
// StreamClosed whose `reason` is the §6 byte when the host refuses or fails.

const alone = await direct(hostRecord);           // §3.4: a session, no signaling
session.connect('ssh');                            // §10.3: a named service

// §10.4: a page answering, serving `ssh` to whoever called.
const a = await answer(callerRecord, { services: { ssh: (stream) => { /* … */ } } });
// a.record goes back to the caller through signaling.
const served = await a.session;

// The same for every call a server of doc/DRT-Signalling.md holds for
// one name: the event stream, polling by cursor, and an answer each.
// With `pair`, the server may also tell this page whom to call (§6.2
// there); the page calls, serves `ssh` on that session too, and reports.
const l = listen('https://signal.example/v1/page', { token, services: { ssh }, pair: '*' });
```

- **Signaling is the page's**, with one exception. The module makes and
  takes records and never opens a WebSocket. `listen` is the exception:
  it is the answerer's half of `doc/DRT-Signalling.md`, and it holds the
  call notification stream and polls while it has none.
- **`accept` resolves once** both channels are open, `hello` has arrived
  and the stream-0 `CONTINUE` has given the initial credit, and rejects
  after 15 s otherwise, closing the connection. One `accept` per `offer`:
  a rejoin is a fresh offer, and so a fresh ufrag (§2.2).
- **Writes are paced twice**: by Wisp credit, one packet per unit, topped
  up by the host's `CONTINUE`; and by the data channel's buffer, past 1
  MiB. A chunk larger than 16379 bytes is split.
- **`closed`** resolves when the target closed cleanly (`0x02`) or the
  page closed the stream, and rejects otherwise. When the session ends
  under a stream, its `reason` is `null`.
- **`scheme` is the page's to act on** (§5). An `https` target means the
  page speaks TLS over the stream itself, and an `ssh` one SSH; the
  library carries bytes. For SSH, `Ssh.connect(s, pinned)` from
  `crates/drt-ssh-web` takes the stream as it is, and
  `script/drt-ssh-page-gate.sh` runs it against a stock sshd in scope.
- **Its gate** is `script/browser-access-client.sh`: `test.mjs` against
  the vectors, then `check.mjs` in Chromium against the `drt-rtc` example
  host and, with `--drt`, against `drt start`. CI runs it with `--drt`,
  and the release packages the files only after it passes.

## 10. Either peer serves, and services have names

§3 to §6 describe one shape: a browser calls, a DRT host answers, and
only the browser opens streams, each to a `host:port` in the host's
scope. This section makes the wire symmetric. It stays v1: a peer that
predates it keeps working with one that follows it, as each rule below
says.

### 10.1 Roles

Every session has a **caller** and an **answerer**. The caller makes the
offer, controls ICE and is the DTLS client; the answerer is controlled
and is the DTLS server. So far the answerer has always been a DRT host
(§3.3). §10.4 adds a browser page as an answerer.

These roles are the connection's and nothing more. **Either peer may open
streams to the other**, and either may serve them; which peer serves SSH,
say, has nothing to do with which one called.

### 10.2 Opening and serving streams

- **Stream ids by parity.** The caller opens odd ids and the answerer
  even ones, so neither ever picks an id the other has open. A peer
  receiving `CONNECT` for an id already open still refuses it with `0x41`
  (§6). A caller that predates this section may open even ids too; that
  is harmless to an answerer that opens none.
- **A peer that serves announces it.** When the channels open, it sends
  `hello` on `control` (§5) and `CONTINUE` on stream 0 with its
  per-stream buffer (§6), as the host always has. Its `hello` lists what
  it serves: `scope` (§5) and `services` (§10.3). A peer that serves
  nothing may send neither. A caller's `accept` still waits for both from
  the answerer.
- **Serving is the same wherever it happens.** The rules of §6 for
  `DATA`, `CONTINUE` and `CLOSE` hold for whichever peer serves: it grants
  credit, the opener spends it, and data towards the opener is paced by
  the channel's buffer.

### 10.3 Named services

A service is something a peer serves under a name rather than at an
address: the page's SSH server is `ssh`.

- **On the wire** it is a `CONNECT` of stream type TCP (`0x01`) with port
  0 and the name as the hostname. A name is 1 to 32 characters of `a-z`,
  `0-9` and `-`, starting with a letter or digit.
- **The server** routes the stream to the service registered under that
  name. An unknown name is `CLOSE 0x48`, as an address out of scope is.
  A server that predates this section refuses port 0 with `0x41` (§6),
  so an opener can tell the two apart.
- **`hello`** lists the names: `"services": ["ssh"]`. An older reader
  ignores the key (§5).
- **A DRT host** names services in its block, each an alias of an entry
  in its scope: `"services": {"ssh": "ssh://127.0.0.1:22"}`. The name
  reaches exactly what the entry does and is checked the same way.

### 10.4 A page answering

A browser cannot take up a connection it was not told about, so a page
that answers always needs signaling (§7): direct mode (§3.4) stays a DRT
host's. Given a caller's record, the page:

1. builds an offer from it, as §3.2 builds an answer, but with
   `a=setup:active` (the caller is the DTLS client) and the caller's
   candidates;
2. applies that, creates its answer, applies it, and waits for gathering
   as §3.1 does;
3. sends its record, read from its answer as §3.1 reads a browser's, back
   through signaling.

The caller then does exactly what it does with a host: builds the answer
from the page's record (§3.2) and waits for `hello` and credit. The page
is the answerer, so it serves (§10.2) and opens even ids. Its session is
ready once both channels are open and the caller's `hello` and credit
have arrived, so a page that was called in order to use what the caller
serves (a DRT host that called it, `doc/DRT-Signalling.md` §6.2) can
`connect` in the first tick; a caller that serves nothing sends neither,
and the session is ready `ANSWER_SETTLE_MS` after the channels opened
with `hello` null. In the library, `answer` and `listen` take `settleMs`.

A page's record changes per session: a browser gathers new candidates
for every connection. Its certificate need not. A page that keeps one
(an `RTCCertificate` survives in IndexedDB) has a fingerprint a caller
can pin across sessions and reloads.
