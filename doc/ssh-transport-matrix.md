# SSH transport matrix

**Status:** target design. This describes every way DRT carries SSH once the
0.8.0 SSH work is complete, written as though all of it exists. The
commands shown are the forms that are built; where one is not built yet,
its form is provisional and changes with its implementation.

SSH is end to end in every row below. DRT never terminates it and never
holds a session key: host-key verification, authentication, `-L`/`-R`,
agent forwarding, `sftp` and `rsync` stay between the SSH client and the
SSH server. DRT supplies the transport, which is one of three things:

- **Direct.** The two ends exchange packets with nothing in between:
  WebRTC (ICE, DTLS, SCTP) or WireGuard. Hole punching is ICE's or
  WireGuard's own handshake. This is the default and the focus.
- **TURN.** WebRTC's standard relay (RFC 8656), which an ICE agent uses on
  its own when no direct pair works. It carries every byte. Browser
  access v1 drops relay candidates from records (`doc/BrowserAccess.md`
  §2.1), so TURN for these rows is a change to that wire, not a setting.
- **Relay.** DRT's WebSocket splice (`doc/Relay.md`): two outbound WSS legs
  joined by label. It carries every byte and reads none of it.

## Order of attempts

A connection tries direct first. It falls back to TURN when a TURN server
is configured, and to the relay when a relay URL is configured. A
configuration that provides neither TURN nor a relay gets direct or
reports a failure to the user, and the failure carries `drt netcheck`'s
verdict for the network it ran on.

## The pieces

| Piece | Runs in | What it is |
|---|---|---|
| `drt tunnel` | native | stdin/stdout transport for `ssh -o ProxyCommand`. Dials a relay (`ws://`, `wss://`) or, with `rtc:`, is the caller in a WebRTC session: `rtc:` and the answerer's record (or a file holding it), or an `http(s)://` signaling URL. `--to` names a service or `host:port`, the service `ssh` if omitted; `--stun` names the STUN servers it asks for its public address. `--park` holds a leg on a relay for a device. |
| `webrtc` block | native, `drt start` | The browser access host (`doc/BrowserAccess.md`): one UDP socket, a fixed record, Wisp streams to the targets in `scope`, for example `ssh://127.0.0.1:22`. `services` names scope entries (`{"ssh": "ssh://127.0.0.1:22"}`); `direct` lets a caller holding the record connect with no signaling. |
| `wireguard` block | native, `drt start` | WireGuard in the process (`doc/WireGuard.md`). Kernel mode makes an interface. Userspace mode needs no privilege and is reached through `forward` and `expose`. |
| `relay` block | native, `drt start` | The relay (`doc/Relay.md`). |
| `ssh.html` | browser | SSH client (`crates/drt-ssh-web`). Connects over a WebSocket URL or over a browser access stream, which it opens from a link: a relay claim URL, a host's record, or a signaling URL that reaches a page. Pins host keys; holds its own Ed25519 key in IndexedDB. |
| page SSH server | browser | An SSH server in a page (`drt-web`). Keys only; an empty authorized list admits nobody. A session gets the page's shell. It runs over any byte stream: a relay leg, or a browser access stream to the page's service `ssh`. |
| `drt_browser_access.js` | browser | The browser half of browser access: `offer` and `accept` to call, `direct` to call a host in direct mode, `answer` for a page that answers, `services` for what a side serves, and `session.connect(service)` or `session.connect(host, port)` as Web Streams. |

The relay speaks URLs and binary WebSocket frames and nothing else, so
`websocat --binary` stands in for `drt tunnel` wherever a relay is the
transport. That is the guarantee that no row needing only a relay depends
on a DRT client.

## Signaling

A direct WebRTC connection needs the two ends' records (§2 of
`doc/BrowserAccess.md`: ICE credentials, DTLS fingerprint, candidates) to
reach each other once. Signaling carries about a kilobyte per session and
none of the traffic. It comes in three modes:

| Mode | Where the records travel | Rows |
|---|---|---|
| **None (direct mode)** | The host's record is fixed, so it travels in a link, a QR code or a config. The caller chooses its own ICE credentials, and the host makes a session from the first connectivity check. | 3 and 5: the answerer is a DRT host with a UDP port the caller can reach, such as a VPS. No server of any kind. |
| **Program** | A `drt start` program behind an `http` listener. `examples/29-browser-access` is a host signaling for itself: a caller `POST`s its record and the reply is the host's. `examples/30-signaling-room` is a room: a meeting point for an answerer that cannot take a request, such as a page. A caller's `POST /call` is held until the answerer, polling `GET /calls`, answers with `POST /answer/<id>`, and the held request's reply is the answerer's record. | Every WebRTC row. Needs a TLS terminator for callers on `https://` pages. |
| **External service** | A third party's API. For Discofetch that is `stdlib:browser-access` on the host's side. | Every WebRTC row, as the service supports it. |

**A page that answers always needs signaling** (rows 6 and 8): a browser
cannot take up a connection it was not told about, so a page answers only
callers whose records reach it through a program or a service. Direct
mode is a DRT host's alone.

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
`ssh://127.0.0.1:22`, named as the service `ssh`. In direct mode the
caller names the callee's record; otherwise it names a signaling URL that
yields it:

```
ssh -o ProxyCommand="drt tunnel rtc:box.record.json" user@box
ssh -o ProxyCommand="drt tunnel rtc:https://box.example/session" user@box
```

`--to 127.0.0.1:22` reaches the same sshd by address. The same host
serves browsers (row 5) with no second configuration.

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
const session = await (await offer()).accept(hostRecord);  // after signaling
const ssh = await Ssh.connect(session.connect("ssh"), pinned);
```

No certificate is involved: DTLS is checked against the fingerprint in
the host's record. In direct mode (`"direct": true` in the block) the
record is all the page needs, and `ssh.html` takes it in a link:

```
ssh.html#rtc=<the host's record>&user=me&hostkey=SHA256:…
```

### 6. Native `ssh` to a browser page

The page runs the SSH server as its service `ssh`, polls a room for
callers, and answers each one. The native side calls through the room:

```
ssh -o ProxyCommand="drt tunnel rtc:https://signal.example/call" user@page
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
page runs the SSH server and answers through a room, as in row 6; the
other runs `ssh.html`, which calls through the same room:

```
ssh.html#call=https://signal.example/call&user=me&hostkey=SHA256:…
```

Across a network each page needs a STUN server for its public address,
since a browser hides its local ones. A relay fallback is row 7 with
`ssh.html` as the caller.

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
- `doc/BrowserAccess.md`: the record, the data channels, Wisp, signaling,
  direct mode (§3.4), and either peer serving named services (§10).
- `doc/WireGuard.md`: both WireGuard modes, and what hole punching relies
  on there.
- `examples/29-browser-access` and `examples/30-signaling-room`: the two
  signaling programs.
