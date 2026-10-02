# DRT signalling, v1

**Status:** draft, 2026-10-01. `examples/30-signaling-room` is the
reference server (§9); `examples/29-browser-access` serves the caller's
request (§3) for a host that is its own answerer.

Signalling is how two peers that cannot yet reach each other swap the one
record each needs to connect directly (`doc/BrowserAccess.md` §2). It
carries about a kilobyte per session and none of the traffic. This is the
profile DRT owns, so that a caller, an answerer and a signalling server can
each be written by somebody else and still meet.

**HTTP is the protocol.** Every record moves in a plain HTTP request, and
an answerer that only polls is correct. A server may also offer a
**call notification stream** (§5): the response to `GET /v1/<name>/events`,
which stays open and carries one Server-Sent Event, a **call
notification**, each time a call arrives. A notification says only that
there is something to read and carries no record. A lost notification
costs latency, never correctness.

## surface

- Roles: the **caller** (§3), which can make a request; the **answerer**
  (§4), which may be unable to take one (a browser page is the case this
  exists for); the **server** (§2), which both reach.
- Configurable values a server states: the hold time (§3), the call
  lifetime (§4.1), the waiting cap (§6), the call notification stream's keepalive (§5).
- Fan-out: the five requests of §2's table.

## 1. Names, tokens and records

- **An answerer has a name** on a server: 1 to 64 characters of `a-z`,
  `0-9`, `-` and `.`. Callers address it by that name; the server never
  passes a caller's record to anyone else.
- **Two tokens per name**, as the relay has two keys per label
  (`doc/Relay.md`): an **answerer token**, which may read calls and answer
  them, and a **caller token**, which may only call. Each travels as
  `Authorization: Bearer <token>`, or as `?k=<token>` where a header
  cannot be set (a browser's `EventSource` cannot set one). A server that
  admits anyone may accept no token; it says so in its documentation, not
  on the wire.
- **A record** is the JSON text of `doc/BrowserAccess.md` §2, at most 512
  bytes. The server does not read it beyond its size: what is in it is the
  two peers' business, and a peer refuses a bad one by the rules of §2.
  A body that is not a record reaches the other peer, which refuses it.

## 2. The requests

All paths are under the server's base URL, which the server documents,
and begin `/v1/<name>/`.

| Request | Who | What |
|---|---|---|
| `POST /v1/<name>/calls` | caller | Body: the caller's record. **Held** until the answerer answers (200, body: the answerer's record) or the hold time passes (504). |
| `GET /v1/<name>/calls?since=<cursor>` | answerer | The calls waiting after `cursor`, and the new cursor (§4). |
| `POST /v1/<name>/calls/<id>/answer` | answerer | Body: the answerer's record. 204; the held call gets it. |
| `DELETE /v1/<name>/calls/<id>` | either | Withdraw a call, or refuse one. The held call gets 410. |
| `GET /v1/<name>/events` | answerer | The call notification stream (§5): `text/event-stream`. |
| `POST /v1/<name>/pair/<id>/result` | answerer | The outcome of a pairing the server asked for (§6.2). 204. |

- **Bodies are sent as `text/plain;charset=utf-8`.** A browser then sends a
  simple request, with no CORS preflight; a server accepts
  `application/json` as well. Replies are `application/json`.
- **CORS:** a server that serves pages answers with
  `access-control-allow-origin`, and answers `OPTIONS` for a client that
  preflights anyway.
- **Status codes**, the same everywhere:

| Status | Meaning |
|---|---|
| 200, 204 | as the table says |
| 400 | a body that is not text, or a malformed path or cursor |
| 401 | no token, where one is required; 403: a token that may not do this |
| 404 | no such name, or no such call (it was answered, withdrawn or expired) |
| 409 | the call was already answered |
| 410 | the call was withdrawn or refused (to the held caller) |
| 413 | a body over 1 KiB |
| 429 | the waiting cap (§6), or a rate limit; `retry-after` says when |
| 503 | no answerer has been seen recently (§4.2): calling now would only wait |
| 504 | the hold time passed with no answer |

## 3. The caller

One request. `POST /v1/<name>/calls` with the caller's record, then wait:
the reply is the answerer's record, or a status that says why not. The
reply carries `location: /v1/<name>/calls/<id>`, naming the call. A held
request has no response head until it is answered, so a caller that gives
up early has no id to `DELETE`: it closes the connection, and its call
lapses at the end of the hold time.

The **hold time** is the server's, at most 30 seconds, and stated in its
documentation. A caller should allow it, plus a margin.

This is WHIP's shape (RFC 9725: `POST`, a `Location` for the session,
`DELETE` to end it, bearer tokens) with records in place of SDP.

## 4. The answerer

### 4.1 Reading calls

`GET /v1/<name>/calls?since=<cursor>` answers:

```json
{"cursor": "17", "calls": [{"id": "c17", "record": "{…}", "expires_in": 24}]}
```

- **The cursor** is opaque: the answerer passes back the last one it was
  given, and gets every call that arrived after it. With no `since`, it
  gets every call still waiting. A cursor is never reused, so a call is
  never missed and never repeated across polls, whatever happened to the
  call notification stream.
- **`calls`** is always an array, empty when nothing waits.
- **`expires_in`** is seconds until the call's hold ends. Answering after
  that is 404.
- The answerer answers each call with
  `POST /v1/<name>/calls/<id>/answer`, or refuses it with `DELETE`.

### 4.2 Being present

A server counts an answerer **present** while it has polled within the
last 30 seconds or holds a call notification stream open. A call to a name with no present
answerer is answered 503 at once, rather than held for an answer that will
not come. An answerer that only polls polls at least that often.

## 5. The call notification stream

`GET /v1/<name>/events`, with the answerer token, is an event stream
(`text/event-stream`, the HTML standard's Server-Sent Events):

```
retry: 2000

id: 17
event: call
data: {"cursor":"17"}

: keepalive
```

- **`event: call`** is a call notification: calls have arrived, and `data` carries the newest
  cursor. The answerer polls (§4.1) with the cursor it already has. The
  event carries no record and no call id: the poll is the only way a call
  is read.
- **`id`** is the cursor, so a browser's `EventSource` sends it back as
  `Last-Event-ID` when it reconnects, and the server may send a call
  notification at once if calls arrived meanwhile.
- **A comment line every 25 seconds** keeps proxies from closing an idle
  stream.
- A server may also offer the same notifications over a WebSocket at
  `/v1/<name>/events` (one text message per notification, the same
  `data`). It changes nothing else.

An answerer that cannot hold a stream polls. One that holds a stream polls
once on connecting, then on each call notification.

## 6. Limits

- A record is at most 512 bytes; a request body at most 1 KiB.
- **The waiting cap**: at most 16 calls wait per name. One more is 429.
- A server may rate-limit calls per name and per address, answering 429
  with `retry-after`.

## 6.1 Claimable names, admission and a caller token

Three optional parts a server may offer; `drt p2p --match` does, and
`examples/30-signaling-room` does not (`doc/P2P.md` §2.4, §7.1).

- **A name is claimed on first use.** The first answerer token to poll a
  free name holds it while that token is present (§4.2); a different token
  gets 403 until the claim lapses, and the same token shares the name.
- **`DRT-Caller-Token: <token>`**, sent by the answerer on its poll, sets
  the name's caller token. Without one, any caller may call the name.
- **`DRT-Accept: <cidr>, …`**, sent likewise, is the range of caller
  addresses the answerer admits; a server that knows the caller's address
  answers 403 outside it (`drt p2p --match` does, by the address its
  listener saw), and the answerer checks the connection's own address as
  well. A bare address is a `/32` or `/128`; an unreadable range admits
  nothing.

## 6.2 Pairing

Two parked peers connect when one calls the other (§3). A server that
pairs peers when neither asked, as a room or a matchmaker does, tells one
of them to call. Optional, like §6.1; a server that offers it documents
so. Decided in `doc/Ask-Discofetch-Reply-2.md`; `doc/P2P.md` §12 holds
what is still open in code.

- **The notification** is a sibling of `calls` in the poll result (§4.1),
  under the same cursor:

  ```json
  {"cursor": "18", "calls": [],
   "pair": [{"id": "p3", "name": "room-7", "server": "https://api.example",
             "token": "<caller token>", "expires_in": 20}]}
  ```

  `name` is who to call, `server` the base to call it at, usually the one
  the answerer is parked at, `token` the caller token that name requires
  (absent when it requires none), and `expires_in` seconds until the
  server stops expecting the call. The call notification stream (§5)
  gains `event: pair`, carrying the cursor and nothing else, exactly as
  `event: call` does. A polling-only server needs only the array. An
  answerer that does not know `pair` ignores the key. Not a new call
  kind: `calls` carries callers' records, and this has a different
  lifecycle.
- **Consent.** An answerer follows a `pair` entry only when its
  configuration allows the name. One value: `*`, any name at the server
  it is parked at; or `drt://<server>/v1/<glob>`, a name pattern at a
  named server. A name pattern without a server is not a value, since
  the risk is not which name but which server the peer is sent to. Off
  by default. The flag and the key are `doc/P2P.md` §2.2 and §8.
- **Who calls.** The server picks one side and tells it. The other side
  needs nothing: it sees an ordinary call in its poll, and its caller
  token and admission range (§6.1) apply as always. The told side is
  parked and serving, and it keeps serving on the session its call
  opens; the browser-access profile is symmetric, so the wire allows
  that (`doc/BrowserAccess.md` §10).
- **The outcome** is reported in one request,
  `POST /v1/<name>/pair/<id>/result`, with the answerer token:

  ```json
  {"outcome": "connected", "why": ""}
  ```

  `outcome` is one of `connected`, `refused` (the called name answered
  the call with a refusal), `unreachable` (the call failed before an
  answer; `why` carries the status, as `503 no answerer is present`), or
  `declined` (the consent rule said no, so the server sees it rather
  than infers it from silence). A result after `expires_in` is 404, as
  an answer after the hold is.

## 7. What a server sees

Both records: addresses, ICE credentials and fingerprints. Never the
traffic, which goes between the peers once they have connected. A server
that wants to read less can be handed less: direct mode
(`doc/BrowserAccess.md` §3.4) needs no server at all.

## 8. Who speaks it

| Party | Role | As |
|---|---|---|
| `drt p2p drt://…/v1/<name>` | caller | stdio or mapped ports over the session (`doc/P2P.md`); `drt p2p --park` is the answerer, `drt p2p --match` a server |
| `ssh.html#call=…` | caller | the SSH page |
| `drt_browser_access.js` | caller, answerer | `offer`/`accept` to call; `listen(base, {token, services})` to answer every call for a name, holding the call notification stream and polling without it |
| a DRT host (`webrtc` block) | answerer | a stdlib program, as `stdlib:browser-access` is for Discofetch's socket |
| a signalling server | server | any HTTP server; `examples/30` is the reference, in dlua |
| `examples/29-browser-access` | server and answerer | §3 only: a host answering every call with its own record as it arrives |

Discofetch can serve this profile on its API beside its own socket
(`doc/BrowserAccess.md` §7.1), with that socket also carrying call notifications, and
every DRT client then reaches a Discofetch-hosted answerer unchanged.

## 9. In dlua

All of it can be written as a `drt start` program. What follows is about
the server's implementation only: every client sees plain HTTP, whichever
way the server is built.

- **The requests of §2** fit DRT's `http` listener. The listener hands the
  program each request as a message and sends the program's reply as the
  response, and the reply may come later, so the held `POST` of §3 is a
  reply the program sends when the answer arrives (within the listener's
  `conn_deadline_ms`).
- **The call notification stream** fits the same listener with
  `streaming` set (`crates/drt/src/listen.rs`). The program answers
  `GET /v1/<name>/events` with a reply carrying `stream = true` and
  `content_type = "text/event-stream"`, keeps that request's `conn`, and
  sends one `chunk` per call notification and one per keepalive. When the
  answerer disconnects, the program gets `{conn, event = "closed"}` on its
  request queue and forgets the `conn`. `examples/30-signaling-room`'s
  `events.dlua` is this, and `examples/31-streaming-responses` is a
  smaller event stream written the same way.
- **One port.** Every request of §2, the stream included, arrives on one
  listener. The stream's keepalive (§5) is shorter than the listener's
  `stream_idle_ms`, so the host never closes a stream the program is
  still holding.
