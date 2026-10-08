# Reflect: a STUN server on UDP and TCP, and a pair of them

`drt p2p --reflect` is the server and `drt p2p drt+reflect://` the
client (`crates/drt/src/p2p/reflect/`); a page asks with `reflect` in
`crates/drt-rtc/client/drt_browser_access.js`. The role is
`doc/P2P.md` §2.7.

## Serving

```
drt p2p --reflect [port] [--host <addr|cidr>]... [--reflect-peer <host[:port]>]... [--reflect-key <key|env:NAME>] [--reflect-rate <n>]
```

- **One port, two protocols.** The default is 3478. UDP answers STUN
  (RFC 5389), with RFC 5780's OTHER-ADDRESS and RESPONSE-ORIGIN; TCP
  answers STUN over TCP with the observed address of the connection.
- **`--host`** is p2p's: `127.0.0.1` (the default) admits this machine,
  `0.0.0.0` anyone, a CIDR that range. Repeatable; each one is bound on
  its own. A request from outside is dropped without a reply.
- **`--reflect-peer`** names the other gate, tried in order, and its
  address is OTHER-ADDRESS. It needs **`--reflect-key`**, which signs
  every request between the gates. A gate with a key and no peer answers
  the other gate's requests and offers no checks of its own.
- **`--reflect-rate`**: cross and change requests a minute from one
  address, 30 by default. The other gate applies it per target as well.
- **The client may name a port, never an address.** Everything the other
  gate sends goes to the address this gate observed.

## What a client can ask

| Request | Over | Answer |
|---|---|---|
| binding | UDP | the mapped address, OTHER-ADDRESS when there is a peer |
| binding | TCP | the observed address and port of the connection |
| CHANGE-REQUEST change-IP | UDP | the other gate answers from its address: filtering (RFC 5780) |
| CHANGE-REQUEST change-port alone | UDP | `not_offered`: there is no alternate port |
| cross, naming port N and a token | TCP | the other gate connects to the observed address on N, writes the token, and the answer is `connected`, `refused`, `timeout` or `unreachable` |

Every answer carries the server's capabilities: `udp`, `tcp`, and with a
peer `peer`, `filtering` and `cross`.

## Asking

```
drt p2p drt+stun://reflect.example       # the UDP checks; port 3478 when absent
drt p2p drt+reflect://reflect.example    # every check the server offers
drt p2p drt+reflect://reflect.example --port 22 --json
```

Both are dispatched on the scheme, before any signalling, and `stun` and
`reflect` are names no service may take. The output is what the gates
answered, one line per check, each with a code below and the answer
beside it, local addresses included; `--json` is the same as one object.
No verdict is attached.

- **udp** and **tcp**: the mapped and the observed address. `drt+stun://`
  runs no TCP.
- **mapping**: the same socket asks OTHER-ADDRESS; one mapped port for
  both is `endpoint_independent`, two are `endpoint_dependent`.
- **filtering**: RFC 5780 from the same socket: `endpoint_independent`,
  or `address_dependent_or_stricter` when no answer gets in, since the
  server has no alternate port to tell the two stricter kinds apart.
- **cross**, per `--port` (up to 4, or one the system picks): this side
  listens on the port, and reports the code and whether the token
  arrived (`received`, `not_received`, or `not_listening` when the port
  was taken here, as by a real service).

`drt netcheck <location>` is `drt p2p drt+reflect://<location>`, and
`--port` and `--json` work as they do there. A `netcheck` block in
`drt start` with `location` (and `port`) pushes the same object to its
queue on every `report_ms`. Every other netcheck flag and key is
deprecated: it still produces the old verdict this release, with a
warning, and leaves with it.

A check the server does not offer reports why (`no_peer`,
`not_offered`) and never fails silently. The run fails only when
nothing answered over UDP or TCP. `--port` and `--json` are flags only,
not `p2p` keys.

## In a page

A reflect port is a plain STUN server, so a page asks it through
WebRTC: `reflect(['reflect1.example', 'reflect2.example'])` gathers
from each server on a connection of its own, then from all of them on
one.

- **udp**: the server-reflexive addresses, or `udp_blocked` when no
  server answered; each server's own answer is in `servers`.
- **mapping**: one address per socket asked together is
  `endpoint_independent`, more is `endpoint_dependent`; `no_peer` with
  fewer than two servers answering.
- **`session.path()`**: on a session to any peer, the path that formed
  and its round trip, from `getStats()`.

A page cannot learn filtering, TCP, a port's reachability or its local
addresses (the browser hides them behind mDNS). Its result is a lower
bound for the native client: a direct path from a page means one
natively, while nothing from a page says nothing certain, since a proxy,
a VPN or browser policy may block the page's UDP.

## Wire

Attributes after the binding message's own, comprehension-optional so a
plain STUN client ignores them:

| Type | Name | Value |
|---|---|---|
| 0xC0D0 | CAPABILITIES | the capabilities, comma-separated |
| 0xC0D1 | CROSS-PORT | the port, then two zero bytes |
| 0xC0D2 | CROSS-TOKEN | 16 bytes the other gate writes on connecting |
| 0xC0D3 | RESULT | a code below |
| 0xC0D4 | PEER-CROSS | gate to gate: the address to connect to |
| 0xC0D5 | PEER-CHANGE | gate to gate: the client's address, then the CHANGE-REQUEST flags; the message's transaction id is the client's |
| 0xC0D6 | PEER-AUTH | gate to gate, last: a millisecond timestamp, a 16-byte nonce, and HMAC-SHA256 over the message with the MAC zeroed |

A gate refuses a request between gates whose MAC fails, whose timestamp
is more than 30 seconds off, or whose nonce it has seen.

## Codes

Every result and failure carries one: the server's in RESULT and as the
reason of a STUN error, the client's on each check's line and in
`--json`. They are stable.

| Code | Means |
|---|---|
| `ok` | the check ran; its answer is beside the code |
| `connected` | the other gate's connection to the port was accepted |
| `refused` | the other gate's connection to the port was refused |
| `timeout` | the other gate's connection to the port got no answer |
| `unreachable` | the other gate has no route to the observed address |
| `sent` | the other gate sent the change request's answer |
| `no_peer` | the server names no peer gate, and the check needs one |
| `no_key` | the gate asked holds no `--reflect-key` |
| `rate_limited` | over `--reflect-rate` requests a minute from this address |
| `peer_unreachable` | the peer gate did not answer |
| `peer_refused` | the peer gate refused the signed request: another key, a clock too far off, or a nonce it has seen |
| `bad_request` | the request was malformed, or named port 0 |
| `not_offered` | the server does not offer this check, as its capabilities say |
| `udp_blocked` | no answer over UDP: the server is down, or UDP does not get out |
| `tcp_blocked` | no answer over TCP: the server is down, or TCP to its port does not get out |
| `unresolved` | the location names no address |
| `same_address` | the change request's answer came from the server's own address, as with two gates on one address, so it says nothing about filtering |
