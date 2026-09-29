# SSH transport matrix

**Status:** target design. This describes every way DRT carries SSH once the
0.8.0 SSH work is complete, written as though all of it exists. The
command and URL forms for pieces not yet built (`drt tunnel rtc:…`, direct
mode) are provisional and change with their implementation.

SSH is end to end in every row below. DRT never terminates it and never
holds a session key: host-key verification, authentication, `-L`/`-R`,
agent forwarding, `sftp` and `rsync` stay between the SSH client and the
SSH server. DRT supplies the transport, which is one of three things:

- **Direct.** The two ends exchange packets with nothing in between:
  WebRTC (ICE, DTLS, SCTP) or WireGuard. Hole punching is ICE's or
  WireGuard's own handshake. This is the default and the focus.
- **TURN.** WebRTC's standard relay (RFC 8656), which an ICE agent uses on
  its own when no direct pair works. It carries every byte.
- **Relay.** DRT's WebSocket splice (`doc/Relay.md`): two outbound WSS legs
  joined by label. It carries every byte and reads none of it.

## Order of attempts

A connection tries direct first. It falls back to TURN when a TURN server
is configured, and to the relay when a relay URL is configured. A
deployment that configures neither gets direct or a failure, and the
failure carries `drt netcheck`'s verdict for the network it ran on.

A configuration that provides neither TURN nor a relay gets direct or
reports a failure to the user.

## The pieces

| Piece | Runs in | What it is |
|---|---|---|
| `drt tunnel` | native | stdin/stdout transport for `ssh -o ProxyCommand`. Dials a relay (`ws://`, `wss://`) or a WebRTC peer (`rtc:`). `--park` holds a leg on a relay for a device. |
| `webrtc` block | native, `drt start` | The browser access host (`doc/BrowserAccess.md`): one UDP socket, a fixed record, Wisp streams to the targets in `scope`, for example `ssh://127.0.0.1:22`. |
| `wireguard` block | native, `drt start` | WireGuard in the process (`doc/WireGuard.md`). Kernel mode makes an interface. Userspace mode needs no privilege and is reached through `forward` and `expose`. |
| `relay` block | native, `drt start` | The relay (`doc/Relay.md`). |
| `ssh.html` | browser | SSH client (`crates/drt-ssh-web`). Connects over a WebSocket URL or over a browser access stream. Pins host keys; holds its own Ed25519 key in IndexedDB. |
| page SSH server | browser | An SSH server in a page (`drt-web`). Keys only; an empty authorized list admits nobody. A session gets the page's shell. It accepts any byte stream: an `RTCDataChannel`, a WebSocket, a relay leg. |
| `drt_browser_access.js` | browser | The browser half of browser access: offer, answer, and `session.connect(host, port)` as Web Streams. |

The relay speaks URLs and binary WebSocket frames and nothing else, so
`websocat --binary` stands in for `drt tunnel` wherever a relay is the
transport. That is the guarantee that no row needing only a relay depends
on a DRT client.

## Signaling

A direct WebRTC connection needs the two ends' records (§2 of
`doc/BrowserAccess.md`: ICE credentials, DTLS fingerprint, candidates) to
reach each other once. Signaling carries about a kilobyte per session and
none of the traffic. It comes in three modes, and every WebRTC row works
with each of them:

| Mode | Where the records travel | Use |
|---|---|---|
| **None (direct mode)** | The host's record is fixed, so it travels in a link, a QR code or a config. The caller chooses its own ICE credentials, and the host makes a session from the first connectivity check. | A host with a reachable UDP port, such as a VPS. No server of any kind. |
| **Program** | A `drt start` program behind an `http` listener keeps a room of records and passes `open` and `close` to the `webrtc` block's queues. `examples/` carries one. | A fetchpoint anyone runs. Needs a TLS terminator for callers on `https://` pages. |
| **External service** | A third party's API. For Discofetch that is `stdlib:browser-access` on the host's side. | Rooms, presence and accounts that belong to the service. |

WireGuard rows need the peer's public key and endpoint instead. They
travel in the config, or through a program that reads them from a room.

## The matrix

| # | Caller | Callee | Transport | Path | DRT at the caller | DRT at the callee |
|---|---|---|---|---|---|---|
| 1 | `ssh` | `sshd` | relay | relayed | `drt tunnel` or `websocat` | `drt tunnel --park` |
| 2 | `ssh` | `sshd` | WireGuard | direct | `wireguard` block | `wireguard` block |
| 3 | `ssh` | `sshd` | WebRTC | direct or TURN, then row 1 | `drt tunnel rtc:` | `webrtc` block |
| 4 | `ssh.html` | `sshd` | WebSocket | direct (reachable) or relayed | none | `websocat`, or a relay and `drt tunnel --park` |
| 5 | `ssh.html` | `sshd` | WebRTC | direct or TURN, then row 4 | none | `webrtc` block |
| 6 | `ssh` | page SSH server | WebRTC | direct or TURN, then row 7 | `drt tunnel rtc:` | none (the page) |
| 7 | `ssh` | page SSH server | relay | relayed | `drt tunnel` or `websocat` | none (the page parks a leg) |
| 8 | `ssh.html` | page SSH server | WebRTC | direct or TURN, then row 7 | none | none |

"Then row N" is the relay fallback, taken when a relay URL is configured.
"None" means no DRT process. A page counts as none because its runtime is
the page itself.

### 1. Native to native, through a relay

```
drt tunnel --park "wss://relay.example/park/box?k=$PARK_KEY" --to 127.0.0.1:22
ssh -o ProxyCommand="drt tunnel wss://relay.example/s/box?k=$CALLER_KEY" user@box
```

For a device with no inbound address and no punchable NAT. With
`websocat --binary` as the ProxyCommand, the caller needs no DRT.

### 2. Native to native, over WireGuard

Both ends run a `wireguard` block. In userspace mode the callee `expose`s
`10.9.0.2:22` to `127.0.0.1:22` and the caller `forward`s
`127.0.0.1:2222` to `10.9.0.2:22`:

```
ssh -p 2222 user@127.0.0.1
```

Kernel mode gives the callee a tunnel address and `ssh user@10.9.0.2`
works as it would on a LAN. One tunnel carries every port, not only SSH.

### 3. Native to native, over WebRTC

The callee runs `drt start` with a `webrtc` block whose `scope` includes
`ssh://127.0.0.1:22`. The caller names the callee's record, or a signaling
URL that yields it:

```
ssh -o ProxyCommand="drt tunnel rtc:box.record.json --to 127.0.0.1:22" user@box
```

The same host serves browsers (row 5) with no second configuration.

### 4. Browser to `sshd`, over a WebSocket

For a server the browser can reach. On the server, behind a TLS
terminator:

```
websocat --binary ws-l:127.0.0.1:8080 tcp:127.0.0.1:22
```

and in the page, `Ssh.connect("wss://server.example/ssh")`. A relay on the
same machine with `drt tunnel --park … --to 127.0.0.1:22` does the same job
with per-label keys. Through a relay elsewhere, it is row 1 with a browser
as the caller.

### 5. Browser to `sshd`, over WebRTC

The server runs the `webrtc` block from row 3. The page opens a browser
access session and runs SSH over one of its streams:

```js
const session = await accept(hostRecord);
const ssh = await Ssh.connect(session.connect("127.0.0.1", 22), pinned);
```

No certificate is involved: DTLS is checked against the fingerprint in
the host's record. In direct mode the record is all the page needs.

### 6. Native `ssh` to a browser page

The page runs the SSH server and offers a WebRTC session. The native side
opens the byte stream towards the page:

```
ssh -o ProxyCommand="drt tunnel rtc:https://signal.example/rooms/r1/page" user@page
```

What the session gets is the page's shell: every `drt` verb the page's
build carries, and whatever the page has exposed to it.

### 7. Native `ssh` to a browser page, through a relay

The page parks a leg on the relay the way `drt tunnel --park` does, and
parks a fresh one each time a leg is claimed:

```
ssh -o ProxyCommand="drt tunnel wss://relay.example/s/page?k=$CALLER_KEY" user@page
```

### 8. Browser to browser

Both ends have WebRTC built in, so no DRT process is in the path. One
page runs the SSH server; the other runs `ssh.html` over an
`RTCDataChannel` between them. Signaling is any of the three modes, and
a relay fallback is row 7 with `ssh.html` as the caller.

A page whose shell exposes the document (a DOM snapshot, console output,
evaluation) is debuggable from anywhere this row reaches.

## What each party sees

| Party | Sees |
|---|---|
| Signaling | Both records: candidate addresses, ICE credentials, DTLS fingerprints. No traffic. |
| TURN, relay | Addresses, timing and byte counts. SSH ciphertext only. |
| The `webrtc` host | Which targets each session opened, with byte counts (`webrtc_stream`). Never payload. |
| The SSH server | Everything, as it always has. |

## Related

- `doc/Relay.md`: the relay's URLs, keys and control plane.
- `doc/BrowserAccess.md`: the record, the data channels, Wisp, signaling
  and direct mode.
- `doc/WireGuard.md`: both WireGuard modes, and what hole punching relies
  on there.
