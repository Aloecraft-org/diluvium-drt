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

A connection is direct, or it fails and says so (`doc/P2P.md` §1). Nothing
falls back on its own: `drt p2p --fallback <relay>` tries direct first and
goes through the relay only when no path reached the peer, and `--relay
<relay>` goes through it always. A failure carries `drt netcheck`'s verdict
for this side's network, measured against the call's `--stun` servers. TURN is not used by these rows: browser access
v1 drops relay candidates from records.

## The pieces

| Piece | Runs in | What it is |
|---|---|---|
| `drt p2p` | native | One verb for peer-to-peer sessions (`doc/P2P.md`). **Call**: `drt p2p <peer>` is the stdin/stdout transport for `ssh -o ProxyCommand`, the caller in a WebRTC session; the peer is a `drt://host/v1/<name>` address at a signalling server, `drt+ssh://` to open the service `ssh`, a record (or a file holding one) for direct mode, or an `http(s)://` URL. `-p` maps ports, `--fingerprint` holds the answerer to a DTLS fingerprint, `--stun` names STUN servers, `--relay`/`--fallback` ask for a carrier. **Listen**: `drt p2p --listen <port>` serves `--forward` on a fixed record, the REPL by default. **Park**: `drt p2p --park drt://…/v1/<name>` answers calls at a signalling server, or holds a leg at a `wss://` relay. **Match**: `drt p2p --match <port>` is the signalling server. `drt tunnel` is an alias for one release. |
| `drt ssh` | native | An interactive SSH client on this terminal, and the REPL's `:ssh`. Straight to `host:port` over TCP, or `--via` a relay claim or `rtc:<peer>` as `drt p2p` reaches them. Trusts `~/.ssh/known_hosts`, asking about a host it has not seen; signs in with the agent, `~/.ssh` keys, then a password. `:ssh` alone in the REPL is the config's `host:ssh/shell` instead: the scope's host, key and pinned host key. In a page (`drt-term.js` with `{ ssh }`), the same command and `:ssh` run `ssh.html`'s client, `--via` only (doc/Browser.md). |
| `webrtc` block | native, `drt start` | The browser access host (`doc/BrowserAccess.md`): one UDP socket, a fixed record, Wisp streams to the targets in `scope`, for example `ssh://127.0.0.1:22`. `services` names scope entries (`{"ssh": "ssh://127.0.0.1:22"}`); `direct` lets a caller holding the record connect with no signaling. |
| `wireguard` block | native, `drt start` | WireGuard in the process (`doc/WireGuard.md`). Kernel mode makes an interface. Userspace mode needs no privilege and is reached through `forward` and `expose`. |
| `relay` block | native, `drt start` | The relay (`doc/Relay.md`). |
| `ssh.html` | browser | SSH client (`crates/drt-ssh-web`). Connects over a WebSocket URL or over a browser access stream, which it opens from a link: a relay claim URL, a host's record, or a signaling URL that reaches a page. Pins host keys; holds its own Ed25519 key in IndexedDB. |
| `ssh` listener | native, `drt start` | The REPL over SSH (SPEC §9): a stock `ssh` client signs in with a key and gets a REPL instance holding that key's grants. Keys come from `principals` (their own grants, inside the deployment's) and an `authorized_keys` file, `~/.ssh/authorized_keys` by default (what `drt repl` here would hold). The host key is `identity.host_key_path`. The same server as the page's (`crates/drt-sshd`). |
| page SSH server | browser | An SSH server in a page (`drt-web`). Keys only; an empty authorized list admits nobody. A session gets the page's shell. It runs over any byte stream: a relay leg, or a browser access stream to the page's service `ssh`. |
| `drt_browser_access.js` | browser | The browser half of browser access: `offer` and `accept` to call, `direct` to call a host in direct mode, `answer` for a page that answers, `listen` to answer every call a signalling server holds for a page, `services` for what a side serves, and `session.connect(service)` or `session.connect(host, port)` as Web Streams. |

The relay speaks URLs and binary WebSocket frames and nothing else, so
`websocat --binary` stands in for `drt p2p --relay` wherever a relay is the
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
| **Program** | A `drt start` program behind an `http` listener, speaking `doc/DRT-Signalling.md`. `examples/29-browser-access` is a host signaling for itself: a caller `POST`s its record to `/v1/box/calls` and the reply is the host's. `examples/30-signaling-room` is a server for an answerer that cannot take a request, such as a page. A caller's `POST /v1/<name>/calls` is held until the answerer, polling `GET /v1/<name>/calls` or told by its call notification stream, answers, and the held request's reply is the answerer's record. | Every WebRTC row. Needs a TLS terminator for callers on `https://` pages. |
| **External service** | A third party's API. For Discofetch that is `stdlib:browser-access` on the host's side. | Every WebRTC row, as the service supports it. |

**A page that answers always needs signaling** (rows 6 and 8): a browser
cannot take up a connection it was not told about, so a page answers only
callers whose records reach it through a program or a service. Direct
mode is a DRT host's alone.

WireGuard rows need the peer's public key and endpoint instead. They
travel in the config, or through a program that serves them.

## The matrix

| # | Caller | Callee | Transport | Path | DRT at the caller | DRT at the callee |
|---|---|---|---|---|---|---|
| 1 | `ssh` | `sshd` | relay | relayed, by request | `drt p2p --relay wss://…` or `websocat` | `drt p2p --park wss://… --forward ssh://127.0.0.1:22` |
| 2 | `ssh` | `sshd` | WireGuard | direct | `wireguard` block | `wireguard` block |
| 3 | `ssh` | `sshd` | WebRTC | direct; `--fallback` to row 1 | `drt p2p <peer>` | `drt p2p --listen` or `--park` with `--forward ssh://…`, or a `webrtc` block |
| 4 | `ssh.html` | `sshd` | WebSocket | direct (reachable) or relayed | none | `websocat`, or a relay and `drt p2p --park wss://…` |
| 5 | `ssh.html` | `sshd` | WebRTC | direct | none | as row 3's callee |
| 6 | `ssh` | page SSH server | WebRTC | direct; `--fallback` to row 7 | `drt p2p drt+ssh://…` | none (the page) |
| 7 | `ssh` | page SSH server | relay | relayed, by request | `drt p2p --relay wss://…` or `websocat` | none (the page parks a leg) |
| 8 | `ssh.html` | page SSH server | WebRTC | direct | none | none |

Nothing falls back on its own: a relayed row is one the caller asked for
with `--relay`, or reached with `--fallback` after no direct path was
found. "None" means no DRT process. A page counts as none because its
runtime is the page itself.

### 1. Native to native, through a relay

```
drt p2p --park "wss://relay.example/park/box?k=$PARK_KEY" --forward ssh://127.0.0.1:22
ssh -o ProxyCommand="drt p2p --relay wss://relay.example/s/box?k=$CALLER_KEY" user@box
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

The callee serves its sshd as one target: `drt p2p --listen 5000 --forward
ssh://127.0.0.1:22` prints a record, and a caller holding it needs no
signalling; `drt p2p --park drt://signal.example/v1/box --forward
ssh://127.0.0.1:22` answers calls by name instead. A `drt start` with a
`webrtc` block naming the service `ssh` serves the same thing beside a
program.

```
ssh -o ProxyCommand="drt p2p box.record.json" user@box
ssh -o ProxyCommand="drt p2p drt://signal.example/v1/box --fingerprint SHA256:…" user@box
```

With no `--forward`, the callee serves its REPL instead (`doc/P2P.md` §5).
The same host serves browsers (row 5) with no second configuration.

### 4. Browser to `sshd`, over a WebSocket

For a server the browser can reach. On the server, behind a TLS
terminator:

```
websocat --binary ws-l:127.0.0.1:8080 tcp:127.0.0.1:22
```

and in the page, `Ssh.connect("wss://server.example/ssh")`. A relay on the
same machine with `drt p2p --park wss://… --forward ssh://127.0.0.1:22`
does the same job with per-label keys. Through a relay elsewhere, it is row 1 with a browser
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

The page runs the SSH server as its service `ssh`, reads its calls from
a signalling server, and answers each one. The native side calls through
the same server, with the caller token:

```
ssh -o ProxyCommand="drt p2p drt+ssh://signal.example/v1/page --H auth=$CALLER_TOKEN" user@page
```

What the session gets is the page's shell: every `drt` verb the page's
build carries, and whatever the page has exposed to it.

### 7. Native `ssh` to a browser page, through a relay

The page parks a leg on the relay the way `drt p2p --park wss://` does, and
parks a fresh one each time a leg is claimed:

```
ssh -o ProxyCommand="drt p2p --relay wss://relay.example/s/page?k=$CALLER_KEY" user@page
```

### 8. Browser to browser

Both ends have WebRTC built in, so no DRT process is in the path. One
page runs the SSH server and answers through a signalling server with
`listen` from `drt_browser_access.js`, as in row 6; the other runs
`ssh.html`, which calls through the same server and reaches the
answerer's service `ssh`:

```
ssh.html#call=<https://signal.example/v1/page/calls?k=…, encoded>&user=me&hostkey=SHA256:…
```

The same link to a DRT host's signalling, such as
`examples/29-browser-access`'s `/v1/box/calls`, is row 5 without direct
mode: the host serves no named service, so `to` or the host's scope picks
the target, as for a record.

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
- `doc/DRT-Signalling.md`: the signalling profile.
- `examples/29-browser-access` and `examples/30-signaling-room`: the two
  signaling programs.
