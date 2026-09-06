# WireGuard in DRT

**Written 2026-09-06, against `claude/wireguard-gotatun`.** What landed,
what it cost, what it does not do yet, and the measurements behind each.

DRT already had a ladder for reaching a host that is not directly
reachable: `netcheck` says what a network can do, `stun` measures the
mapping that decides it, the relay carries what cannot be reached
directly, `turn` carries what a browser needs. Every rung answers the same
question — *can these two hosts exchange packets* — and none of them
answered *and then what*. The answer was a WSS tunnel per TCP connection,
which works, and which puts a relay in the path of every byte.

This is the other answer. Once two hosts can exchange UDP datagrams,
which is exactly what `netcheck`'s `punchable` verdict means, carrying
arbitrary traffic between them is a solved problem, and the solution is
about 4000 lines of very well-studied protocol with no per-stream setup,
no relay in the path, and roaming for free. A deployment gets an address
for a fetchpoint, and `ssh/exec`, `rest` and `listen` reach it with no new
plumbing at all — because it is just an IP address.

---

## surface block

1. What shipped, and how to configure it.
2. The punch, which is the reason the block has a reply queue.
3. What it cost: four measurements.
4. What it does not do.

---

## 1. What shipped

A `wireguard` block, a `drt wg` verb, and a bridge inside `drt start`,
all on [gotatun](https://github.com/mullvad/gotatun) — Mullvad's
maintained fork of Cloudflare's boringtun, MPL-2.0, audited.

The block is `wg-quick`'s field names on purpose. An operator who has
written a `[Interface]`/`[Peer]` pair should transcribe rather than
translate, and a key pasted from one should work in the other:

```lua
return {
  supervisor = "sup.lua",
  wireguard = {
    listen_port     = 51820,          -- the port netcheck --udp-port measures
    interface       = "drt0",
    private_key_env = "WG_KEY",       -- or private_key_file, or private_key
    queue           = "wg_in",        -- reports land here
    reply_queue     = "wg_out",       -- commands are read here (see §2)
    report_ms       = 10000,
    peers = {
      {
        public_key  = "…base64…",
        allowed_ips = { "10.9.0.2/32" },
        endpoint    = "203.0.113.7:51820",   -- optional; see §2
        keepalive   = 25,
      },
    },
  },
}
```

`drt wg` runs it in the foreground and prints the public key, because a
peer cannot be configured without it and the alternative is running
`wg pubkey` against a secret on a terminal. Inside `drt start` the same
device reports peer stats on the timer and endpoint changes as they
happen.

**The privilege.** Creating the tunnel interface needs `CAP_NET_ADMIN` (or
root) on Linux, root on macOS, and `wintun.dll` beside the binary on
Windows. That is the *whole* privilege: no kernel module, no `wg` tools,
no `wg-quick`, no second daemon.

**Every refusal is at startup**, with the thing that was wrong named: a
key that is not 32 base64 bytes, a CIDR that is not a network, an
endpoint that is not a `host:port`, an unset environment variable, a port
already held, an interface this process may not create. The two silent
failures worth refusing loudly are a truncated key (a device nobody can
reach) and a swapped public and private key (a device anybody can be),
and neither announces itself at run time —
`a_key_that_is_not_a_key_is_refused_by_name`.

---

## 2. The punch, and why the block has a reply queue

A punched peer **has no endpoint until the rendezvous supplies one**, and
the rendezvous is a program's business, not a config's. So the shape is:

1. Both sides run `netcheck --udp-port 51820` — the same port the
   `wireguard` block listens on, because a NAT mapping is measured per
   port, and measuring an ephemeral one tells you about a socket you are
   not going to use.
2. `punchable` on both sides means the mapping is endpoint-independent:
   the address a STUN server saw is the address the other peer can reach.
3. The two deployments trade those addresses over the relay — a program's
   own code, using the relay that already exists.
4. Each tells **its own** device where the other turned out to be, by
   pushing one message onto `reply_queue`:

```lua
queue.push(wg_out, {
  command    = "endpoint",
  public_key = their_key,
  endpoint   = "198.51.100.4:51820",
})
```

5. Both sides send; the handshakes cross; the hole is open. `keepalive`
   holds it open, and is the other command for the same reason — a
   punched mapping with no traffic through it closes again in tens of
   seconds.

The program learns it worked rather than assuming it: the report carries
`last_handshake_ms` per peer, and **an endpoint with no handshake is an
address nobody answered at**. Roaming is reported separately, as
`wireguard_endpoint`, because a peer that moved between two snapshots
would otherwise look like it had never moved.

`reply_queue` is empty by default. A config that did not ask to be
steered is not steerable, and a queue read by mistake would be a
program's own messages consumed by a subsystem.

Nothing here performs the rendezvous. Everything here makes one
sufficient.

---

## 3. What it cost

Four measurements, all from this machine on 2026-09-06.

| | |
|---|---|
| `full` before | 6,153,752 bytes |
| `full` with `wireguard` | 6,535,576 bytes |
| delta | **+381,824 bytes, +6.2%** |
| new dependencies | 105 crates in gotatun's tree, most already here |

**No new C toolchain.** `default-features = false` drops gotatun's
default `aws-lc-rs` for `ring`, which rustls already links. That matters
beyond size: `aws-lc-sys` needs cmake and a C toolchain per target, and
that is precisely what keeps `full` off Windows and linux aarch64 today.
Taking it on for WireGuard would have spread the problem instead of
containing it.

**It cross-compiles to Windows.** `slim,wireguard` for
`x86_64-pc-windows-gnu` with mingw builds clean, which was not obvious
and is the reason Windows is a maybe rather than a no (§4).

**Rust 1.95 is the floor**, from gotatun. Only for this feature: `slim`
and every other profile still build on older toolchains, and a build
without the feature on 1.94 says so precisely —
`gotatun@0.9.2 requires rustc 1.95`. CI's `stable` is well past it.

**And it works.** `crates/drt/tests/wireguard.rs` runs two devices in one
process — two loopback UDP sockets, a real handshake, a real IPv4 packet
in one end and the same bytes out the other, in both directions. Nothing
below the tunnel is mocked: if the handshake or the cryptokey routing
were wrong, no packet would arrive.

That test needs no privilege, which is the whole reason gotatun was worth
choosing over linking a tunnel daemon. Its `Device` is generic over
**both** transports:

- The **UDP side** is a trait, so DRT can hand the device the socket
  `netcheck` measured rather than have it bind its own behind our back
  and get a different mapping.
- The **IP side** is a trait, so `drt start` gives it a kernel interface
  and the tests give it a pair of channels. Same device, same code path,
  proven in CI.

A second test covers the punch itself: a peer with no endpoint is
measurably unreachable, one `endpoint` command later the packet arrives,
and the report shows the handshake —
`a_peer_with_no_endpoint_is_unreachable_until_the_endpoint_command_arrives`.

---

## 4. What it does not do

- **No no-root mode.** The device still wants a kernel interface, so a
  deployment needs `CAP_NET_ADMIN`. The pieces for a userspace mode are
  all present — the IP side is a trait, and the tests already drive it
  with channels — so a `--local`-shaped local port terminating TCP
  in-process over a userspace stack (smoltcp) is the next change, and it
  touches only the IP side of what is here. That is also §8's "reliable
  stream over the UDP hole" from `doc/Ask-Discofetch-Reply.md`, answered
  with WireGuard instead of QUIC.
- **No rendezvous.** §2 is a shape, not a service. The exchange is a
  program's, over the relay.
- **No route management.** DRT creates the interface; it does not assign
  addresses to it or add routes. `ip addr`/`ip route`, or the deployment's
  own `exec`, still do that. Worth folding in, and deliberately not
  guessed at here.
- **Not on Windows yet.** The cross-build works, but `full` does not
  build for Windows for unrelated reasons (`exec` is unix-only,
  aws-lc-sys through russh), and `wintun.dll` would have to ship beside
  the binary. `doc/Platforms.md` has the state of that.
- **Not in `wasi` or `web`.** Neither has a tunnel interface, and the
  wasm targets have no threads to drive one.
