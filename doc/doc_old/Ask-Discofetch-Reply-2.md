# DRT's reply to Discofetch's two asks against dev.17

**Written 2026-10-02, against `claude/drt-dlua-ssh-tunnel-cdflop` at
697cf2c.** Answers two asks Discofetch raised against `doc/P2P.md` and
`doc/DRT-Signalling.md` as of v0.8.0-dev.17 (21fa3bb). Same convention as
`doc/Ask-Discofetch-Reply.md`: `landed` names the commit and the test;
`open` says what it would take and where it stands; a correction is kept
in place rather than quietly folded in.

**Short version.** Ask 2 is landed: dev.18 sends `scope` as an array on
every `hello` and the `webrtc` block shows its entries by default again,
so a generated host config produces the dev.11 wire and
fetchpoint-browser-client dev.13 needs no change. Dev.17's "not a `v`
bump" was wrong and is corrected below. Ask 1 is decided here and landed
the same day: the poll result gains a `pair` array, consent is one value,
the told side calls and keeps serving, and it reports the outcome back.
`drt p2p --match` offers the ask, so the whole loop runs on loopback.

---

## surface block

1. Pairing started by the signalling server: decided, landed.
2. `hello.scope`: landed, with a correction.
3. The note on hold time.

---

## 1. Pairing started by the signalling server (P2P.md §12) — decided, `landed`

The shapes below are in `doc/DRT-Signalling.md` §6.2 and are final; the
server half can be built against them now, and tested against
`drt p2p --park --pair` as the told side.

### The notification

Not a new call kind. `calls` carries offers from callers; an instruction
to call someone has a different lifecycle. The poll result gains a
sibling array under the same cursor:

```json
{"cursor": "18",
 "calls": [],
 "pair": [{"id": "p3", "name": "room-7", "server": "https://api.example",
           "token": "<caller token>", "expires_in": 20}]}
```

`server` is a base; it is usually the one the peer is parked at, and it is
there so the consent rule has something to match. The notification stream
gains `event: pair` carrying only the cursor, exactly as `event: call`
does. A polling-only server needs nothing beyond the array. A parked side
that does not know `pair` ignores the key.

### Consent

Off by default. One value, as `--pair` on `drt p2p --park` and the key
`pair` on the block (`doc/P2P.md` §2.2, §8):

- `*`: any name at the server I am parked at. This is the room case and
  the value a generated config will carry.
- `drt://<server>/v1/<glob>`: a name pattern at a named server, for
  anything cross-server.

A bare name pattern without a server is not accepted. The risk is not
which name but which server the peer is told to reach; same-server
pairing adds nothing the server could not already do by handing the peer
an offer.

### Who answers whom

The server picks one and tells it. The other side needs nothing new: it
sees an ordinary call in its poll, and its caller-token and `DRT-Accept`
checks apply as they do today. The told side is parked and serving and
now also calls, and it keeps serving on the session that results. The
wire already allowed that (the profile is symmetric, `doc/BrowserAccess.md`
§10); the host now does too: `Command::Call` in `drt-rtc` is the same
session with the ICE, DTLS and SCTP roles flipped.
`a_host_that_calls_a_page_serves_it_all_the_same` in
`crates/drt-rtc/tests/host.rs` has a page answer a host's call and reach
the host's named service over it.

### Failure

Reported, in one request:

```
POST /v1/<name>/pair/<id>/result
{"outcome": "connected" | "refused" | "unreachable" | "declined",
 "why": "503 no answerer is present"}
```

`declined` is the consent rule saying no, so the server sees it rather
than infers it from silence. A 503 on the call itself is `unreachable`
with the status in `why`. A result after `expires_in` is 404, as an
answer after the hold is.

### What landed — `landed`

The parked side reads `pair`, `event: pair` wakes its poll, `--pair`
(and the key `pair`) is the consent, and the outcome goes back as above.
`drt p2p --match` offers the ask, `POST /v1/<name>/pair {"name": other}`
with `<name>`'s answerer token, tells `other`, and records the result.
`the_match_server_pairs_two_parked_peers_when_one_asks` in
`crates/drt/tests/p2p.rs` runs the loop on loopback: three parked peers
at one match server, one asks for a call from a peer with `--pair *` and
gets it, then from a peer without `--pair` and sees `declined`. The ask
request is one form of asking; a room decides for itself and needs none.

*Later the same day.* The told side was `drt p2p --park` only when the
above was written. A page is one now: `listen(base, {pair})` in the
browser library follows a `pair` entry under the same consent value,
calls with the entry's token, serves on the session and reports;
`crates/drt-rtc/browser-check/pairing.mjs` proves it in Chromium against
`drt p2p --match` and a parked `drt p2p`, and a page without `pair`
declines there too. A page that was called in order to use what the
caller serves can `connect` at once (`doc/BrowserAccess.md` §10.4). For
a `drt start` deployment with a `webrtc` block, whose program does its
own signalling, `{command = "call", peer, rtc}` makes the host the
calling side; whether to follow a room's instruction is that program's.
Both told sides bound the whole follow, the caller's request included,
by the entry's `expires_in` less a margin, so a call that never connects
is reported `unreachable` before the hold ends rather than answered 404
after it.

## 2. `hello.scope` optional — `landed`, with a correction

### Which of the three — `landed`

The first two together, in da438d0 (dev.18). `scope` is an array on every
`hello`: the entries when the block's `hello_scope` is on, `[]` when off.
`default` is absent when hidden. The `webrtc` block's `hello_scope` is on
by default, so a host from a generated config sends the dev.11 wire and
dev.13 of the client parses it unchanged. Only `drt p2p` hosts send `[]`.
`hello_names_the_caps_the_host_was_given_and_nothing_when_none` and
`one_forward_takes_every_stream_whatever_port_it_asks_for` in
`crates/drt-rtc/tests/forward.rs` hold the empty form; the host suite
holds the shown form. The browser library's `.d.ts` says
`scope: ScopeEntry[]`.

### Correction, kept in place

Dev.17's `doc/P2P.md` §7.2 said omitting `scope` was "not a `v` bump
because a v1 client already has to cope with a host whose scope is one
entry". That was wrong. Every v1 client indexed `scope`, so omitting it
was a break, and the client's throw was correct against the wire it was
built on. The doc now says `scope` is always sent and why.

### The rule for a page with an empty scope — confirmed

The asked reading is the intended path: ask for the named service when
`hello.services` lists one, else `CONNECT` with an empty host and port 0
for whatever the host forwards to. `default` never comes back without
`hello_scope`. This path is exercised only by `drt p2p` hosts and by a
deployment that turns `hello_scope` off; Discofetch's hosts keep sending
`scope` and `default`. Implement it, and expect not to see it from that
edge.

### The navigation boundary — decided

Scope was never the boundary. `doc/BrowserAccess.md` §5 already said the
host enforces host and port only; a service worker that fences
navigation on `hello.scope` is trusting a hint. §5 now says:

- A page's boundary is what the session was opened for. A page opened
  for the service `ssh` routes to that stream and nothing else. A page
  opened for the empty target routes to that and nothing else.
- A page does not attempt a destination it was not opened for. If it
  does, the host's `CLOSE 0x48` is what enforces the line, and the page
  shows the refusal.
- When `scope` is present it is the list the operator chose to show: fit
  for a picker, not a rule the page relies on.

For the dev.13 client this is a small change: keep building the picker
from `scope` when it is non-empty, and stop treating the absence of an
entry as what blocks a navigation. The opened service or target is the
only destination.

## 3. The note

A 20 second hold against a 25 second listener deadline is fine, and §1
of `doc/DRT-Signalling.md` already says a polling-only server is correct.
Nothing to decide.
