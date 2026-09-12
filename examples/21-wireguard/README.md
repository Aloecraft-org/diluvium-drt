# 21-wireguard

A WireGuard peer in the process — the parts that need no privilege: the keys,
the config, and what a config that cannot work is answered with.

`22-wireguard-interface` is the other half, which creates the interface and
needs `CAP_NET_ADMIN`.

## Run it

```
cd examples/21-wireguard
drt wg keygen
drt wg check --config wrong.json
drt wg check --config hub-unroutable.json
drt wg check --config rendezvous.json
```

## What you should see

```
$ drt wg keygen
<a key, 32 bytes base64>          # the private key
<a key, 32 bytes base64>          # its public key, to give a peer

$ drt wg check --config wrong.json
drt wg check: wireguard.address: '10.9.0.0/24' is the network address, not a host
address on it. Give the address this device holds, like 10.9.0.1/24.

$ drt wg check --config hub-unroutable.json
drt wg check: peer p6Vqzz…= is allowed 192.168.1.0/24, and nothing will reach
it: outside 10.9.0.1/24, the only prefix the interface routes. It will
handshake and carry nothing. Add the route yourself (`ip route add
192.168.1.0/24 dev drt0`), or narrow allowed_ips.
ok: drt0 on port 51820, 1 peer(s), 1 warning(s)

$ drt wg check --config rendezvous.json
ok: drt-fp on port 51820, 0 peer(s)
    no peers named: it will create drt-fp, give it 10.9.0.1/24, measure its
    mapping, and wait for `add` on wg_out
```

## What it teaches

**`drt wg keygen` is why nothing else is needed.** Every other WireGuard
setup begins with `wg genkey | wg pubkey`, from a package you now depend on.
Private key first, public key second, so `drt wg keygen | head -1` is a key
and nothing else — a key printed among prose is a key someone pastes with the
prose.

**`fp.json` and `hub.json` are the two ends, and they mirror.** One
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
`wrong.json` carries three mistakes and trips the first. JSON has no
comments, so the other two sit beside it as `_wrong_2` and `_wrong_3` --
keys the loader ignores, saying what to write; swap one in at a time:

- `10.9.0.0/24` — a route's network address where a host address belongs. The
  commonest transcription slip, and it would otherwise yield an interface
  that answers to nothing.
- `"listen_port": 0` — an ephemeral port cannot be named to a peer, and a NAT
  mapping belongs to a port, so it could never be measured either.
- one `stun` server — one server can report an address; it takes two to say
  whether it *changed*, which is what decides whether a hole punch is
  possible at all.

**`drt wg check` is how you find out without root.** Creating the interface
needs `CAP_NET_ADMIN`; being told the config is wrong should not. `check`
runs everything `drt wg` runs before it touches the interface and stops, so
a config can be written and checked on a laptop and only deployed where it
is allowed.

**The failure it exists for is the silent one.**
`hub-unroutable.json` has nothing wrong with it and will not work: DRT
gives the interface its `address` and the kernel derives exactly one route
from that, the on-link `10.9.0.0/24`. The peer is also allowed
`192.168.1.0/24` — everything behind the hub — and no route points there, so
WireGuard encrypts for it happily and nothing ever hands it a packet. The
tunnel comes up, reports a handshake, and carries nothing.

Both numbers are in the config, so `check` says which peer, which network,
and the `ip route add` that fixes it. It is a **warning and exit 0**, not a
refusal: the config is correct and works the moment the route exists, and a
non-zero exit would fail a deploy over a note. Route management is not DRT's
(`doc/WireGuard.md` §4).

**A block with no peers is a config, not an oversight.**
`rendezvous.json` names none, because a device that can only talk to
peers already written into its config is a device that never needed a
rendezvous. It comes up knowing nobody, measures its own mapping against
the two STUN servers, publishes what it finds, and waits to be told about a
peer on `reply_queue`. `check` says exactly that back rather than leaving
`0 peer(s)` to read like something was forgotten — and it says so loudly if
there is no `reply_queue`, because then nothing can ever tell it.

**`drt wg pubkey` is the half `keygen` cannot give you.** A key that already
exists — in a secret store, in a `[Interface]` stanza — still has to be
nameable to a peer, and `drt wg keygen` only makes new ones. `pubkey` reads
a private key on stdin and prints its public half, which is `wg pubkey`
exactly.

**MTU 1420, not 1500.** WireGuard's own overhead is 60 bytes over IPv4 and 80
over IPv6, so an interface left at the ethernet default fragments every
full-size packet.

`doc/WireGuard.md` is the long version, including what is and is not proven
about hole punching.
