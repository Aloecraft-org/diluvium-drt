# DRT signalling, v1

**Status:** draft, 2026-10-01. Not yet implemented: `examples/29` and `examples/30`
speak earlier, unversioned shapes of this, and are brought into line with
it (§9).

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
response carries `location: /v1/<name>/calls/<id>`, so a caller that gives
up early can `DELETE` it.

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

## 7. What a server sees

Both records: addresses, ICE credentials and fingerprints. Never the
traffic, which goes between the peers once they have connected. A server
that wants to read less can be handed less: direct mode
(`doc/BrowserAccess.md` §3.4) needs no server at all.

## 8. Who speaks it

| Party | Role | As |
|---|---|---|
| `drt tunnel rtc:https://…/v1/<name>/calls` | caller | stdio over the session (`doc/ssh-transport-matrix.md`) |
| `ssh.html#call=…` | caller | the SSH page |
| `drt_browser_access.js` | caller, answerer | `offer`/`accept`, and an answerer helper that holds the call notification stream and falls back to polling |
| a DRT host (`webrtc` block) | answerer | a stdlib program, as `stdlib:browser-access` is for Discofetch's socket |
| a signalling server | server | any HTTP server; `examples/30` is the reference, in dlua |

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
  request queue and forgets the `conn`. `examples/31-streaming-responses`
  is an event stream written this way.
- **One port.** Every request of §2, the stream included, arrives on one
  listener. The stream's keepalive (§5) is shorter than the listener's
  `stream_idle_ms`, so the host never closes a stream the program is
  still holding.
