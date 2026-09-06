# 21-wireguard

A WireGuard peer in the process — the parts that need no privilege: the keys,
the config, and what a config that cannot work is answered with.

`22-wireguard-interface` is the other half, which creates the interface and
needs `CAP_NET_ADMIN`.

## Run it

```
cd examples/21-wireguard
drt wg keygen
drt wg --config wrong.host.lua
```

## What you should see

```
$ drt wg keygen
<a key, 32 bytes base64>          # the private key
<a key, 32 bytes base64>          # its public key, to give a peer

$ drt wg --config wrong.host.lua
drt wg: wireguard.address: '10.9.0.0/24' is the network address, not a host
address on it. Give the address this device holds, like 10.9.0.1/24.
```

## What it teaches

**`drt wg keygen` is why nothing else is needed.** Every other WireGuard
setup begins with `wg genkey | wg pubkey`, from a package you now depend on.
Private key first, public key second, so `drt wg keygen | head -1` is a key
and nothing else — a key printed among prose is a key someone pastes with the
prose.

**`fp.host.lua` and `hub.host.lua` are the two ends, and they mirror.** One
peer's `address` is the other's `allowed_ips`; one's `listen_port` is the
other's `endpoint` port; each names the other's *public* key and only ever
its own private one, by environment variable. Read them side by side — the
symmetry is the whole mental model.

**`wg-quick`'s field names, on purpose.** An `[Interface]`/`[Peer]` stanza
transcribes rather than translates, and a key pasted from one works in the
other. What is not `wg-quick`'s is that the interface comes up *ready*: the
`address` and `mtu` are set and the link brought up, so the common case needs
no `ip addr add`.

**A config that cannot work is refused before anything binds.**
`wrong.host.lua` carries three mistakes and trips the first; uncomment the
others one at a time:

- `10.9.0.0/24` — a route's network address where a host address belongs. The
  commonest transcription slip, and it would otherwise yield an interface
  that answers to nothing.
- `listen_port = 0` — an ephemeral port cannot be named to a peer, and a NAT
  mapping belongs to a port, so it could never be measured either.
- one `stun` server — one server can report an address; it takes two to say
  whether it *changed*, which is what decides whether a hole punch is
  possible at all.

**MTU 1420, not 1500.** WireGuard's own overhead is 60 bytes over IPv4 and 80
over IPv6, so an interface left at the ethernet default fragments every
full-size packet.

`doc/WireGuard.md` is the long version, including what is and is not proven
about hole punching.
