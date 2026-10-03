# `drt p2p`: one verb for peer-to-peer sessions

**Status:** built, 2026-10-01, with the decisions of the first review
written in (§11): `crates/drt/src/p2p/` is the verb, `stdlib:p2p-match`
the server, and `crates/drt/tests/p2p.rs` drives every role on loopback.
Not built: the relay's "answer for me" service (§4.3) and TLS flags for
`--match`. It replaces `drt tunnel` (§9), and folds the WebRTC caller (`drt tunnel rtc:`), the
relay's park and claim, and a reference signalling server into one verb.
It builds on `doc/BrowserAccess.md` (the record, Wisp, direct mode §3.4,
named services §10), `doc/DRT-Signalling.md`, and
`doc/ssh-transport-matrix.md`, and changes each of them (§7).

`drt tunnel` grew five modes, four of which are a relay. This verb inverts
that: the bare command is direct, and every way a third machine can enter
the path is a flag the user typed.

## surface

- Entry points: four roles. **Call**, `drt p2p <peer>`. **Park**,
  `drt p2p --park <signalling>`. **Listen**, `drt p2p --listen <port>`.
  **Match**, `drt p2p --match <port>`. Two carriers, on call and park:
  `--relay <peer>` and `--fallback <peer>`.
- Configurable: `-p` (call), `--host` (default `127.0.0.1`), `--accept`,
  `--pair`, `--forward`, `-A`, `-P`, `--signal`, `--stun`,
  `--fingerprint` (`--fingerp`), `--H` (`auth=` for a bearer token),
  `--authorized-keys`, `--capacity`, `--extra-root`, `--config`; `--show`
  prints a peer's canonical form. Every flag is also a key of the `p2p`
  block (§8).
- Fan-out: the role table (§2), the peer address forms (§3), the forward
  targets and how a requested port meets them (§5), the admission table
  (§6), and the `drt tunnel` map (§9).

## 1. The promise

> With no `--relay` and no `--fallback`, no machine other than the two
> ends carries a byte of the session. When no such path exists,
> `drt p2p` fails and says so.

This sentence is the first line of the verb's help, and every rule below
serves it.

- **A signalling server is not in the path.** It carries one record each
  way, about a kilobyte, once, and never a byte of the session. A user who
  does not know the far end's address is exactly who it serves.
- **TURN and relays are in the path.** Both carry every byte, so neither is
  used without a flag. This removes the matrix's implicit fallback ("then
  row N" whenever a relay URL is configured): falling back is something an
  invocation asks for, never something a config implies.
- **A failure says why.** It names the path that failed and the reason.
  A call with no carrier whose session does not come up also says what
  this side's network is: `drt netcheck`'s verdict, measured against the
  call's own `--stun` servers (two are needed, and with fewer the clause
  says so). Only this side's: the far side's NAT is the other half of the
  answer, and nothing here can measure it.
- **The promise is per side.** Each side's flags govern its own path. A
  caller with no `--relay` may reach a parked side that chose one; the
  caller's path is then direct to that relay. The relay says so in its
  `hello` (§4.4) and the caller prints it, so a direct-looking command
  never hides a relayed session.

## 2. Roles

| Role | Form | Reachable through | Serves |
|---|---|---|---|
| call | `drt p2p <peer>` | (it calls) | nothing; stdio or `-p` ports are its end |
| park | `drt p2p --park <signalling>` | a signalling server, by name | `--forward`, or the REPL |
| listen | `drt p2p --listen <port>` | its record; with `--signal`, its own signalling port | `--forward`, or the REPL |
| match | `drt p2p --match <port>` | it is the signalling server | nothing |

### 2.1 Call

```
ssh -o ProxyCommand="drt p2p drt://signal.example/v1/mypc --fingerprint SHA256:…" me@mypc
ssh -o ProxyCommand="drt p2p --config mypc.json" me@mypc
drt p2p drt://signal.example/v1/mypc -p 8080:80 -p 5432:5432
```

The positional is a peer address (§3). The process is the caller in a
browser access session.

**By default the caller's end is stdio**, as `drt tunnel`'s was: one
stream, stdin to the far side and the far side to stdout. That is the
ProxyCommand form.

**`-p <local>:<remote>` maps ports** in the shape of `ssh -L` and
`docker -p`, and is repeatable. The caller binds `<local>` on loopback and
gives each accepted connection its own stream, asking the far side for
`<remote>`: a port, a service name (`-p 2222:ssh`, what `drt+ssh://` says
for stdio), or `host:port` for a peer that serves a scope by address, as
`ssh -L port:host:hostport` does. `-p :<remote>` leaves the local side
out, which is stdio asking for that `<remote>`; `-p <local>` alone leaves
the remote out, which is a local port to whatever the far side forwards
to.

- **A native caller that cannot bind `<local>` fails**, naming the port
  and the reason, as any program does when its port is taken or
  privileged.
- **A page has no ports to bind**, so it reads only `<remote>`. One config
  then serves a native caller and a page alike.
- **`<remote>` is needed only against a port set.** A far side whose
  `--forward` names one target sends every stream there (§5.1), so the
  ProxyCommand form needs no `-p` at all: stdio is the default, and the
  far side's target is the destination.

A port the far side does not serve behaves as a closed port does anywhere
else: the stream is refused, and nothing more is said.

### 2.2 Park

```
drt p2p --park drt://signal.example/v1/mypc --H auth=<answerer token>
drt p2p --park drt://signal.example/v1/mypc --H auth=… --accept 203.0.113.0/24 --forward ssh://127.0.0.1:22
drt p2p --park drt://signal.example/v1/room-1 --park drt://signal.example/v1/room-2 --H auth=…
```

The parked side is the answerer of `doc/DRT-Signalling.md`: it holds the
call notification stream (§5 there), polls by cursor, and answers each
call. It is the native counterpart of `listen` in
`drt_browser_access.js`. The one-liner is for quick use; a long-running
device runs the same thing as `drt --config mypc.json p2p` under a
process supervisor; a `p2p` block is not yet served by `drt start`.

`--accept <cidr>` admits only callers from that range. It is an
instruction to the signalling server, which refuses other callers before
their records reach the parked side (§7.1). The parked side also checks
the remote address of the selected ICE pair itself, so the rule holds
against a server that ignores it. The two addresses are usually the same
NAT, but not always: a caller behind an HTTP proxy signals from one
address and connects from another.

`--park` repeats: one answerer for several names, at one server or
several, with one record and one `--forward` behind them all. Each name
is its own answerer to its server, with its own poll and stream, and
every name is sent the same `--H` headers, so names at one server share
an answerer token. A name its server refuses for good (401 or 403) stops
being answered and the rest carry on; the park ends when none is left.
In a config, `park` is a string or a list. A `wss://` leg parks alone.

`--fingerprint` does not apply: a parked side takes calls from many
callers, so there is no one fingerprint to hold it to.

`--pair <allow>` lets the server pair this side with another parked peer
(`doc/DRT-Signalling.md` §6.2): `*` for any name at the server it is
parked at, or `drt://<server>/v1/<glob>` for a name pattern at a named
server. Told to call, this side makes the caller's request with the token
the entry gives, serves on the session as it serves a caller's, and
reports the outcome. Without `--pair`, a `pair` entry is declined and the
server is told so.

**Two parked peers reach each other by one calling the other.** Parking
and calling are not exclusive: a machine parked as `a` runs
`drt p2p drt://signal.example/v1/b` to reach the peer parked as `b`, with
`b`'s caller token if `b` set one. The signalling server matches the call
as it matches any other, and nothing new is needed in the profile. Both
peers are usually behind NAT, so both need a public address from STUN
(§2.5); a pair of NATs that defeats ICE, such as a symmetric NAT on both
sides, gets no direct path and fails as §1 says, unless the caller asked
for `--fallback`. A page does the same with `listen` and `offer` from
`drt_browser_access.js`.

### 2.3 Listen

```
drt p2p --listen 5000                                   # loopback only, serves the REPL
drt p2p --listen 5000 --host 0.0.0.0 --forward 127.0.0.1:8080
drt p2p --listen 5000 --host 10.9.0.0/24 --forward ssh://127.0.0.1:22 --signal 5001
```

The listening side binds UDP on `<port>` with a fixed record, in direct
mode (`doc/BrowserAccess.md` §3.4). **By default it takes no signalling at
all.** It prints its record, and the `drt p2p` command that calls it with
the record in hand, so a caller needs nothing but the UDP port. Opening
one port for one protocol is all a firewall or a router needs.

**`--signal` also serves the caller's request of the signalling profile**,
for a caller that has an address and not a record. `--signal 5001` takes
it on that TCP port; a bare `--signal` takes it on a free port the system
chooses, and prints the `drt://host:port` a caller uses. The record the
request returns carries the UDP port in its candidates, so a caller only
ever needs to know the signalling port.

### 2.4 Match

```
drt p2p --match 8443 --host 0.0.0.0 --capacity 256
```

A reference signalling server for `doc/DRT-Signalling.md`, built from
`examples/30-signaling-room`'s program, with its settings taken from flags
instead of `args`. The example and the verb are one implementation, so the
profile has one server to test against. Real deployments with accounts,
rooms or long-lived ownership of names write their own; this one shows
what the profile asks of them.

- **Names are claimed on first use.** A parked side that polls a free name
  with an answerer token holds that name with that token while it is
  present (30 seconds after its last poll, or while it holds the
  notification stream, profile §4.2). A different token gets 403 until the
  claim lapses. The same token shares the name: every holder sees each
  call, the first answer wins, and the others get 409. That makes
  redundant parked sides free.
- **A caller token is optional**, set by the parked side when it claims
  (§7.1). Without one, any caller may call that name. The profile allows a
  server that admits anyone if its documentation says so; `--match`'s help
  says so.
- **`--capacity`** is the number of names held at once. A claim past it
  gets 429 with `retry-after`. Each name keeps the profile's cap of 16
  waiting calls.
- **Admission is by token, not origin**, so every reply carries
  `access-control-allow-origin: *`. A page opened from `file://` sends
  `Origin: null` and preflights because of its `Authorization` header; a
  server that answered only its own `https://` origin would lock every
  such page out of named peers while the same page served from that
  origin worked. The same holds for a `--listen` peer's `--signal` port.
- **A name routes; it does not identify.** Anyone may claim a free name, so
  reaching `mypc` proves nothing about who answered. `--fingerprint` on the
  caller is what proves it.
- **TLS** is not built in. A page on an `https://` origin cannot call a
  plain `http://` server, so the help says to put a TLS terminator in
  front, as example 30's config does. Certificate flags can come later.
- `--match` with `--forward`, `--relay` or `--fallback` is refused: a
  signalling server carries no session bytes (§10).

### 2.5 Public addresses

`--stun <host:port>`, repeatable, names the STUN servers a side asks for
its public address, and every role that makes a WebRTC session takes it:
call, park and listen. Without one, a side offers only its local address,
which a peer reaches only on the same network or when that side's NAT
admits packets it did not ask for. `--stun` is also the `stun` key of the
`p2p` block, the same list the `webrtc` block takes, so every role
gathers the same way.

## 3. Peer addresses

The positional of a call, the argument of `--relay` and `--fallback`, and a
`--forward drt://…` target are all one type, a peer address:

| Form | Means |
|---|---|
| `drt://host[:port]/v1/<name>` | the caller's request of `doc/DRT-Signalling.md` §3 at that server, for that name |
| `drt+<service>://…` | any `drt://` form, then a stream to the far side's named service, such as `drt+ssh://` |
| `drt://host:port` | the same request to a `--listen` peer's `--signal` port |
| `host…` with no scheme | `drt://`, unless it names a file that exists: then that file holds a record (the row below) |
| `https://…/v1/<name>/calls` | exactly that request |
| a record, or a file holding one | direct mode: nothing is sent |
| `wss://…` | the WebSocket relay, as a carrier only (§4.3) |

`drt://` means the two ends are DRT and DRT chooses how they talk. Today
that is the profile's request over HTTPS, and over HTTP for a loopback
address.

**One canonical form per peer.** Every spelling above that names the same
peer converges on one string: a signalling address as its base with the
scheme resolved and no query or `/calls`, a record as `record:` and its
fingerprint, a relay URL without its key. `drt p2p --show <peer>` prints
it, and `canonicalPeer` in the browser library computes the same, so
anything keyed by peer (a launcher's stored token or pinned fingerprint)
has one key and nobody ports the parser.

**The record as a file.** Inside a project, `--listen` also writes its
record to `.drt_root/live/p2p-<port>.record.json`, rewritten when it
changes and removed when a `-` session ends, so a launcher on the same
machine calls the peer in direct mode with nothing sent.

**A named service goes in the scheme.** RFC 3986 allows `+` in a scheme,
so `drt+ssh://signal.example/v1/mypc` calls the same peer as
`drt://signal.example/v1/mypc` and opens its service `ssh`
(`doc/BrowserAccess.md` §10.3). Service names already fit a scheme's
characters: 1 to 32 of `a-z`, `0-9` and `-`. A scheme is case-insensitive,
so DRT lowercases it before matching. A named service is one target, so
a `-p` remote port beside it is not needed (§5.1). A page serves only
named services, so this is how a caller picks one of several; a native
serving side's `ssh://` forward is the service `ssh` too.

DRT never has to tell a destination from a signalling server. Every URL
is a signalling endpoint, a server's or the peer's own, and both answer
the same request with the same thing: the answerer's record.

## 4. Carriers

### 4.1 `--relay <peer>`

```
drt p2p --relay drt://relay.example:5001 drt://signal.example/v1/mypc
```

1. Call the relay as any peer is called.
2. Once the control channel is up, send the destination, the positional,
   inside the session: `{"t":"call","to":"<peer address>"}` on `control`.
3. The relay calls the destination itself and joins the two sessions:
   every Wisp packet from the caller goes to the destination as it is,
   and every packet from the destination to the caller, so the relay reads
   nothing and credit stays between the two ends. It answers
   `{"t":"called","hello":<the destination's hello>}` on `control`, or
   `{"t":"failed","why":…}` and ends the session. A stream opened before
   the destination answers waits; one opened before a destination is
   named is refused as a closed port is.

The destination never appears in a URL, and the relay holds no label and
no state for it in advance. A relay is any `--listen` or `--park` peer
with a bare `--forward` (§5). The signalling server that put a caller in
touch with a relay cannot tell it was one.

### 4.2 `--fallback <peer>`

Try direct first. If ICE finds no pair, do what `--relay` does. The
session reports which path it took.

### 4.3 The parked side behind a relay

`--relay R --park S` asks R to answer S's calls on the parked side's
behalf: the parked side polls S as usual, hands each caller's record to R,
and posts R's record back as its answer. That is a second relay service,
"answer for me", beside 4.1's "call for me". **It is deferred.** Until
then, a parked side that cannot use UDP at all, as on a network that
blocks STUN, holds a WebSocket leg to the relay instead:

```
drt p2p --park wss://relay.example/park/mypc?k=… --forward ssh://127.0.0.1:22
```

That is today's `drt tunnel --park` under the new verb, and the one place
the WebSocket relay carries a p2p session. The WebSocket relay otherwise
stays for its two existing users, `websocat` as a ProxyCommand and a page
that parks a leg (matrix rows 1, 4 and 7), and nothing new is pointed at
it.

### 4.4 The relay announces itself

A peer with a bare `--forward` adds `"forwarding": true` to its `hello`.
A caller prints `via relay` when it sees it, whatever its own flags were.

## 5. What a serving side serves

`--listen` and `--park` serve one of:

| `--forward` | Serves |
|---|---|
| (absent) | the REPL, through the built-in SSH server of SPEC §9 |
| `127.0.0.1:8080` | one TCP target |
| `ssh://127.0.0.1:22` | one target as the named service `ssh` |
| `127.0.0.1 -P 80,8080:8090,31200` | those ports on that host |
| `127.0.0.1 -A` | every port on that host |
| `127.0.0.1` | an error at startup naming `-A` and `-P` |
| `drt://…` | another DRT peer: the forwarder calls it and joins the sessions |
| `-` | this process's stdio, one session at a time, as netcat listens |
| (bare) | a relay: the caller names the destination (§4.1) |

- **The REPL default** is SPEC §9's sshd: russh, public keys only, the PTY
  channel attached to a REPL instance, and authorized keys mapping to
  capability grants in root config. Built: `crates/drt-sshd` is the server
  (the page's, moved), and `drt start`'s `ssh` listener serves the REPL on
  it; `--listen` and `--park` run the same in-process. A key in the
  running user's `~/.ssh/authorized_keys` gets what `drt repl` there
  would hold, which is sshd's stance on the account's own list; a key in
  `principals` gets that principal's grants, inside the deployment's. It
  is the native counterpart of the page's SSH server in `drt-web`, and it
  means a host with no other process is still useful.
- **`-P`** takes a comma-separated list of ports and `low:high` ranges. A
  malformed list is an error, never a guess.
- **`--forward -`** pairs with a caller's stdio for moving a file or a
  pipe: `tar c . | drt p2p <peer>` on one end, and
  `drt p2p --listen 5000 --forward - > backup.tar` on the other.
- **A `drt://` target need not know what is at the end of the chain.** The
  forwarder makes an ordinary call and joins two sessions; a relay, a page
  or a service answers its own side.
- **Bare `--forward` reaches whatever the caller names.** That is the
  relay's job, and the flag is the opt-in; the help says so. A config
  narrows it with the `webrtc` block's `scope`, which already refuses a
  stream outside it by host and port.

Each `--forward` target becomes a scope entry, and `ssh://` becomes the
named service `ssh` (`doc/BrowserAccess.md` §10.3), so the host's existing
checks enforce all of this unchanged.

### 5.2 The REPL as the service `repl`

The same shape in a page: `DrtTerm.repl(cols, rows, sink, closed)` in
the browser module is the in-page root's REPL as a byte stream with
`input`, `resize` and `close`, so a launcher attaches a terminal to a root
one way whether the root is the page's own module or a `drt` reached over
WebRTC (`doc/Browser.md`).

Beside `ssh`, the REPL default serves the named service **`repl`**: the
PTY's bytes on the Wisp stream itself, and the terminal's size as a
message on `control`, `{"t":"resize","stream":<id>,"cols":<c>,"rows":<r>}`,
sent by the peer whenever its terminal changes. A page attaches with
`drt_browser_access.js` and a terminal widget, with no SSH client and no
key in a keystore, which is what a launcher's own terminal wants, and so
does a native caller: `drt p2p <peer> -p :repl`.

What differs from `ssh` is who is admitted. Over `ssh` a key signs in and
gets that key's grants. Over `repl` **the session is the gate**: whoever
may reach this peer (holds its record, passed its `--host` or `--accept`,
or was admitted by the signalling server) gets what `drt repl` on that
machine would hold. That is the right posture for a record handed to one
person or a loopback launcher, and the wrong one for `--signal` on an
address anyone can reach, so a listening peer says so when it serves the
REPL that way.

### 5.1 How a requested port meets `--forward`

Only the serving side routes, and only by what its own `--forward` says.
A caller's `-p` never causes an error by itself.

| `--forward` serves | A stream that asks for a port | A stream that asks for none |
|---|---|---|
| one target, a named service the caller asked for by `drt+<service>://`, the REPL, `-`, or a `drt://` peer | goes to that target; the port is ignored | goes to that target |
| a port set, `-P` or `-A` | goes to that port if the set holds it, else is refused as a closed port | `-P` with exactly one port: that port. Otherwise refused as a closed port |
| a relay, bare `--forward` | mirrored to the destination as it was asked, so the destination's row applies | mirrored likewise |

A refusal is the Wisp `CLOSE` a blocked stream gets today. The caller
learns that the port is closed and nothing about what the far side does
serve: a `-P` set is the serving side's policy, not routing information.

A stream that asks for none is a Wisp `CONNECT` with an empty host and
port 0: "whatever you forward to". A stream that asks for a port and
names no host is the same `CONNECT` with that port: "whatever you forward
to, at this port". A named service is still port 0 and the service's
name, as today, so `drt+<service>://` needs nothing new. Hosts older than
this refuse the empty form as malformed; every host is upgraded with this
change, so nothing negotiates it.

## 6. Admission and credentials

| Flag | Side | Checked by | What it controls |
|---|---|---|---|
| `--host` | listen, match | the serving process | who may connect: an address binds there; `0.0.0.0` admits anyone; a CIDR, typically a WireGuard subnet, admits that range, and binds this machine's own address inside it when it holds exactly one, everywhere otherwise |
| `--accept` | park | the signalling server, then the parked side | which callers are passed to the parked side |
| `--H name=value` | call, park | sent to the signalling side only | HTTP headers; `auth=` is shorthand for `Authorization: Bearer` |
| `--fingerprint SHA256:…` | call | the caller, locally | the answerer's DTLS fingerprint must match; `--fingerp` is the same flag |

- **`--H` never reaches the peer.** The record exchange is the signalling;
  the session itself carries no HTTP.
- **`--fingerprint` is the defence against the signalling server.** A
  dishonest server could answer with its own record and sit in the middle;
  a caller given a fingerprint refuses any record whose fingerprint does
  not match. Without one the first connection is trust-on-first-use, as an
  unpinned SSH host key is. It is spelled `fingerprint` and not `pin`
  because `pin` already means an SSH host key on `ssh.html` and `drt ssh`,
  and the two are different keys.
- **`--host` defaults to `127.0.0.1`**, so nothing is reachable from
  another machine until the user types an address.

## 7. Changes to other documents

### 7.1 `doc/DRT-Signalling.md`

Three optional parts, for servers that offer them. Example 30 is
unaffected.

- **Claimable names** (§2.4's rules), as a server behaviour the profile
  names rather than leaves to each implementation.
- **An admission range**, sent by the answerer when it claims, which the
  server applies to callers' source addresses and answers 403 outside.
- **A caller token**, sent by the answerer when it claims.
- **Pairing** (§6.2 there): a `pair` array beside `calls` in the poll
  result and an `event: pair` on the stream, telling a parked side which
  name to call, at which server, with what caller token; a consent value
  on the parked side; and `POST /v1/<name>/pair/<id>/result` with the
  outcome.

- `hello` gains `forwarding` (§4.4), and `caps`: the capability names a
  program or REPL behind the host may hold, from the config's ceiling,
  grants only. A host that forwards to a TCP target or another peer sends
  none. It is what a launcher shows as a root's permissions before a
  session; what a session was granted, which the session itself decides
  (§5.2), arrives as `{"t":"granted","stream":N,"caps":[…]}` on `control`
  once the service knows. The browser library settles `stream.granted`
  with it.
- `hello` shows the scope's entries and `default` only when the serving
  side's config asks it to. `drt p2p` does not by default: it names the
  services and nothing about the addresses and ports behind them, which
  are the serving side's policy (§5.1) and, for a forward into a private
  network, a map of that network for anyone admitted. The `webrtc` block
  shows them by default, as every v1 client was written against; a
  deployment that wants them hidden turns `hello_scope` off. `scope` is
  always sent, empty when hidden, so a v1 client that indexes it still
  parses; `default` is simply absent. Not a `v` bump.
- A control-channel message carries a relay's destination (§4.1).
- Half-close: the Wisp profile's `END` packet and the `half_close`
  announcements (`doc/BrowserAccess.md` §6). A caller whose stdin ends
  sends `END` and waits for what the far side still has to say; the
  two-second grace remains only for a peer without it.

### 7.3 Elsewhere

- `doc/ssh-transport-matrix.md`: every row in `drt p2p` terms, and "then
  row N" replaced by `--fallback`.
- `ssh.html`: `#call=` and `#rtc=` are unchanged, and `#call=` also takes
  a `drt+ssh://` address. It asks for the service `ssh` when the `hello`
  names one, and otherwise for no port, instead of choosing a target from
  the `hello`'s scope, which is usually absent. A `fingerprint` parameter
  checks the answerer's DTLS fingerprint as `--fingerprint` does, beside
  the existing SSH `hostkey` pin.
- `drt_browser_access.js`: `listen` is a parked side, and with `pair` a
  told one (`doc/DRT-Signalling.md` §6.2): the same consent value as
  `--pair`, read by `parsePairRule`, the caller's request made with the
  entry's token, the session served, the outcome reported and handed to
  `onPair`. `session.connect` with no arguments asks for no port (§5.1).
  `offer`, `accept` and `direct` take `fingerprint`: a `SHA256:…` string
  compared to the answerer's record, or `fp => Promise<boolean>` asked
  before any session byte flows, so a stored pin is a comparison and a
  missing one is the "trust this peer?" prompt. The digest is the record's
  `f`, so this is glue, not protocol.
- The examples and corpus snapshots that carry a `tunnel` block (11, 19,
  29, 30) move to `p2p`.

## 8. The config block

The `p2p` block replaces `tunnel`, with every flag a key under the block's
name, and a flag and a key that disagree refused as the conflict it is, as
`tunnel` does today. `tunnel` is read as an alias for one release, with a
warning naming the key that replaces each of its own. `--pair` (§2.2) is
the key `pair` under the block. A `webrtc` block has no such key: it does
not park, its program does all its signalling, so whether to follow an
instruction to call is the program's, and `{command = "call", peer, rtc}`
on the block's `reply_queue` is how it does so (`doc/BrowserAccess.md`
§8).

## 9. From `drt tunnel`

| `drt tunnel` | `drt p2p` |
|---|---|
| `rtc:<record or file>` | `<record or file>` |
| `rtc:https://…/v1/<name>/calls` | `drt://…/v1/<name>` or the same URL |
| `--to <port>` on `rtc:` | `-p :<port>` |
| `--to <service>` on `rtc:` | `drt+<service>://…` |
| `--local <addr>` | `-p <local>:<remote>` |
| `wss://…/s/<label>?k=…` | `--relay wss://…/s/<label>?k=…`; the label names the destination, so no positional |
| `--park wss://… --to <target>` | `--park wss://… --forward <target>` (§4.3) |
| `--listen <addr> --to <target>`, the WebSocket to TCP bridge | stays on `drt tunnel` for now, with a warning; its home is the `relay` block, not yet built, since it is a server-side shim and not a peer |
| `--header` | `--H` |
| `--extra-root` | `--extra-root` |

`drt tunnel` stays as an alias for one release, printing the `drt p2p`
form of what it was given.

## 10. Separation

- **A signalling server forwards no bytes.** Its work happens at the
  moment two peers meet; a relay's lasts the whole session. A server doing
  both carries everyone's traffic to serve anyone's handshake.
- **A relay answers no calls it is not forwarding.**
- **Composing them is a dlua program's choice.** Both are `drt start`
  programs underneath, so a deployment that wants one process doing both
  writes it; the verb never does it by default.

## 11. Decided in review

1. **Authorized keys for the REPL default:** the running user's
   `~/.ssh/authorized_keys`, with what `drt repl` there would hold; a line
   carrying options (`from=`, `command=`) is skipped with a warning rather
   than honoured without its restriction. `principals` narrow per key.
2. **`drt://` is HTTPS, and HTTP for loopback.** A LAN peer without a
   certificate is reached by its `http://…` URL, the explicit form in §3.
   `--fingerprint` is what protects the record either way.
3. **`drt://host:port` with no path is `POST /`.** A `--listen` peer's
   `--signal` port answers `/` and `/v1/<any name>/calls` alike.
4. **The headers** are `DRT-Accept: <cidr>, …` and
   `DRT-Caller-Token: <token>`, sent on the answerer's poll. `--accept`
   sets the first; the second is any `--H DRT-Caller-Token=…` the parked
   side chooses to send, since `--H` already reaches the signalling side.
5. **`drt ssh`** takes a peer address as its positional, the user in front
   of it (`drt ssh me@drt+ssh://signal.example/v1/mypc`), or `-u` / `-l`;
   `--relay` and `--fallback` as on `drt p2p`. Built: a target with a
   scheme `drt p2p` takes, a record, or a file holding one rides a call,
   opening the service `ssh` (or `--to`); anything else is TCP as before.
6. **`--match` and example 30.** The verb's server grows from example
   30's program. Example 30 is a declared human surface, one screen per
   file, so what claimable names, `--capacity` and admission add lives
   in the stdlib program the verb runs, and the example stays its
   readable core.
7. **The fingerprint flag is `--fingerprint`**, not `--pin`: `pin` is the
   SSH host key's word on `ssh.html` and `drt ssh`.

## 12. Decided in review, later

1. **Pairing started by the signalling server.** Decided 2026-10-02
   (`doc/Ask-Discofetch-Reply-2.md`), shapes in `doc/DRT-Signalling.md`
   §6.2: a `pair` array in the poll result and `event: pair` on the
   stream tell a parked side which name to call; consent is `--pair`
   (§2.2), off by default; the told side calls and keeps serving; it
   posts the outcome back. Built: the serving host calls as well as
   answers (`Command::Call` in `drt-rtc`, the same session once
   connected, with the ICE, DTLS and SCTP roles flipped), the parked
   side follows a `pair` entry under its `--pair`, and `--match` offers
   the ask (`POST /v1/<name>/pair`). Not a separate call-and-serve role:
   the park is one, when told. A page is one too: `listen` with `pair`
   in the browser library (§7.3), proven in Chromium against `--match`
   and a parked `drt p2p` (`crates/drt-rtc/browser-check/pairing.mjs`).
   A `webrtc` block's program calls with `{command = "call"}` (§8).

## Not in this proposal

- Peer keys for a tunnel adapter: WireGuard public keys over the control
  channel, addresses derived from them, and SSH certificates from a key
  registry. Separate proposal.
- TLS flags for `--match`.
- The relay's "answer for me" service (§4.3).
