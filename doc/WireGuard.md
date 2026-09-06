# WireGuard in DRT

**Written 2026-09-06, against `claude/wireguard-gotatun`.** What landed,
what it cost, what it does not do yet, and the measurements behind each.

**On hole punching, up front:** what is built is the deployment-side half
— measure our own mapping, be told where a peer is, talk to it — and it is
tested without a NAT anywhere in the path. The punch itself relies on
WireGuard's own handshake retransmission, which is sound and is not
something this repository has measured. §2 is the long version, and it is
the section to read first.

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
    listen_port     = 51820,          -- required; never 0, see below
    interface       = "drt0",
    address         = "10.9.0.1/24",  -- put on the interface; no `ip addr` needed
    mtu             = 1420,           -- not 1500: WireGuard's overhead is 60-80
    private_key_env = "WG_KEY",       -- or private_key_file, or private_key
    stun            = { "stun1.example:3478", "stun2.example:3478" },
    turn_fallback   = true,           -- may be asked to relay; see §2
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

`drt wg keygen` prints a fresh pair — private key on the first line, its
public key on the second — so setting up a peer never needs
`wireguard-tools` installed, which was the point. `drt wg pubkey` is the
half it cannot give you: a key that already exists, in a secret store or a
`[Interface]` stanza, still has to be nameable to a peer, and this reads one
on stdin and prints its public half. `drt wg check` reads a config, says
what is wrong with it, and stops — everything `drt wg` does before it
touches the interface, so a config can be written and checked on a laptop
and only deployed where the privilege is. `drt wg` runs the
device in the foreground and prints the public key, because a peer cannot
be configured without it. Inside `drt start` the same device reports peer
stats on the timer, and endpoint changes and first handshakes as they
happen — polled every 250 ms, because watching a punch means watching for
a handshake that either lands within a second or two or never lands at
all.

**`listen_port` is required and may not be zero.** gotatun answers
`listen_port()` with the port it was *configured* with rather than the one
it bound, so an ephemeral port could not be read back to tell a peer; and
a NAT mapping belongs to a port, so a peer that wants to be reached must
own a stable one. Refused at startup with that reason rather than
defaulted quietly.

**The interface comes up ready.** `address` and `mtu` are set on it and
the link is brought up, so the common case needs no `ip` commands at all.
Without an `address` the interface would appear, hold no address, and stay
down — an interface nothing can use, which is the `wg-quick` work this
block exists to replace.

**The privilege.** Creating the tunnel interface needs `CAP_NET_ADMIN` (or
root) on Linux, root on macOS, and `wintun.dll` beside the binary on
Windows. That is the *whole* privilege: no kernel module, no `wg` tools,
no `wg-quick`, no second daemon.

**Every refusal is at startup**, with the thing that was wrong named: a
key that is not 32 base64 bytes, a CIDR that is not a network, a *network*
address where a host address belongs (`10.9.0.0/24` — the commonest slip,
and it yields an interface that answers to nothing), an endpoint that is
not a `host:port`, a peer with no `allowed_ips` (which can neither be
routed to nor accepted from), one STUN server where two are needed, an
unset environment variable, a port already held, an interface this process
may not create. The two silent failures worth refusing loudest are a
truncated key (a device nobody can reach) and a swapped public and private
key (a device anybody can be), and neither announces itself at run time —
`a_key_that_is_not_a_key_is_refused_by_name`,
`a_config_that_cannot_work_is_refused_before_anything_binds`.

---

## 2. The punch: what is built, and how far it is proven

**Read this section before believing anything about hole punching.**

### What a punch needs, and who does each part

1. **Both peers learn their own public mapped address.** STUN. DRT does
   this — see "the measurement" below.
2. **The mapping must be endpoint-independent.** If the NAT allocates a
   fresh mapping per destination ("symmetric"), the address a STUN server
   saw says nothing about what a peer would see, and no punch is possible
   from that side. DRT classifies this and says so — and when the answer is
   no, there is somewhere to go: the TURN fallback below.
3. **They exchange addresses through a rendezvous.** A program's job, over
   the relay that already exists. **DRT does not do this**, and §4 says so.
4. **Both sides send to each other, repeatedly, at about the same time.**
   This is the punch itself. A's first packet to B's public address is
   dropped by B's NAT (no mapping yet) but creates A's; B's first packet
   then finds A's mapping open. **WireGuard does this by itself**: a peer
   with an endpoint and no session retransmits a handshake initiation
   every 5 seconds for 90 seconds, so once both sides have each other's
   endpoint the simultaneous-open falls out of the protocol's own
   behaviour. This is the property the whole approach rests on, and it is
   WireGuard's, not DRT's.

   One thing the lab run below taught, which explains a punch that
   "succeeds" and then never handshakes: **the far peer's first
   unsolicited probe must be dropped, not answered.** A Linux NAT with an
   *open* INPUT chain confirms the conntrack entry for that probe and then
   remaps the outbound flow off the port STUN measured (51820 became 65038
   in the lab), so the address both sides just exchanged is stale before
   either uses it. A router's default-drop is what makes port preservation
   hold. This is a property of the network in the path, not something DRT
   can arrange.

   A second conntrack finding, from a later run of the same lab, is worth
   knowing because it looks like a DRT bug and is not: **a conntrack entry
   outlives a change to the rule that created it.** A NAT switched to
   symmetric keeps answering the *old* port-preserving mapping for the
   flows it was already carrying — so a device that had measured through
   that NAT reads `punchable` and believes it, correctly, about a NAT that
   no longer behaves that way. In a lab it means rebuilding before the
   symmetric scene. In the field it is a router that changes behaviour
   under load, and it is the clearest argument for `remap` being a command
   a program can issue on evidence rather than a measurement taken once at
   startup: the mapping is not a property of the network, it is a property
   of the network *and* the flows already through it.
5. **Keepalives hold the mapping open.** A punched mapping with no traffic
   closes in tens of seconds. `keepalive = 25` — settable in the config
   and in the same command that sets the endpoint.

### What is actually tested

`crates/drt/tests/wireguard.rs` proves the deployment-side plumbing:

- Two devices carry a real IPv4 packet, both ways, over a real handshake.
- A peer with **no endpoint** is measurably unreachable, and one
  `endpoint` command later the packet arrives and the handshake is
  reported.
- A peer the config **never named** is added at run time and then reached
  — the case a rendezvous actually produces.
- The reports a rendezvous cannot start without survive a queue that has
  not been declared yet, and `report_ms` ticks for a device with no peers
  — both found by the lab run below, both recorded in issue #15.

**Every one of them runs on loopback. There is no NAT in any of them.**
So they test that DRT can be told where a peer is and will then talk to
it. They do **not** test that a hole gets punched through two real NATs,
because nothing in this repository has two NATs to punch through. Step 4
above is WireGuard's well-established behaviour, and it is the step this
repository's own tests do not reach.

Calling this "the hole punch, end to end" — as an earlier version of this
document and its commit message did — was an overstatement. In this
repository it is the deployment-side half, tested without a NAT.

### The measurement, and its honest limit

The `stun` field measures this device's own mapping **on `listen_port`,
immediately before the device binds it**, and reports it as
`wireguard_mapping` with the address to publish and whether publishing it
is worth anything.

This exists because `netcheck --udp-port 51820` **cannot do this job**
once a deployment is running: the device already holds the port, so the
probe cannot bind it, and measuring a different port measures a different
mapping. An earlier version of this document claimed DRT "hands the device
the socket `netcheck` measured". It does not. The UDP side of gotatun's
device *is* a trait, so that remains possible, but what is implemented is:

> measure the mapping of a probe socket on the same local port,
> microseconds before the device binds that port for real.

On an endpoint-independent NAT the external mapping is a function of the
internal port, so the answer holds for the device's own socket. On a
symmetric NAT it does not — which is exactly the case the measurement
reports as `punchable: false`, where the relay is the path and no punch
was ever going to work.

### What a program does with it

```lua
-- Learn where we are, from the deployment itself.
if msg.event == "wireguard_mapping" then
  if msg.punchable then rendezvous_publish(msg.address)
  else use_the_relay_instead(msg.why) end
end

-- The rendezvous answered: here is the peer, and where it turned out to be.
queue.push(wg_out, {
  command     = "add",
  public_key  = their_key,
  allowed_ips = { "10.9.0.2/32" },
  endpoint    = their_address,
  keepalive   = 25,
})

-- Did it work? A handshake is the answer; an endpoint with none is an
-- address nobody answered at.
if msg.event == "wireguard" and msg.peers[1].last_handshake_ms then ... end

-- And if the command was wrong, the program that wrote it is told.
if msg.event == "wireguard_error" then log(msg.command, msg.reason) end
```

`reply_queue` is empty by default: a config that did not ask to be steered
is not steerable, and a queue read by mistake would be a program's own
messages consumed by a subsystem.

**Clearing an endpoint is said out loud** — `clear = true`. The first
version read an absent `endpoint` field as "forget where the peer is",
which made one mistyped field name tear down a working tunnel in silence.

**`remap` is how a machine that moved says so.** `wireguard_endpoint`
reports a *peer* roaming; nothing reported us roaming, so a laptop that
went from home Wi-Fi to a phone hotspot had a published endpoint that was
wrong and no event saying so (issue #17 §2).

```lua
-- The network changed under us. Ask again, and re-join the room.
queue.push(wg_out, { command = "remap" })
if msg.event == "wireguard_mapping" then rendezvous_publish(msg.address) end
```

It re-runs the measurement `start` runs and re-emits `wireguard_mapping`,
with the same caveat about the probe's socket. **It costs a rehandshake**,
and it has to: a mapping belongs to a port, measuring one means binding
the port the running device is holding, so the sequence is suspend,
measure, resume — and `resume` resets every peer's session on purpose. For
the case it exists for that is free, because a device whose network just
changed has dead sessions already. It is a command and not an interval for
the same reason.

One measured detail, in case it ever moves: `suspend` returns *before*
gotatun's I/O tasks drop their sockets — about 6 ms — so `remap` waits for
the port to actually come free (`PORT_RELEASE_MS`) rather than measuring
into "address already in use".
`suspending_gives_the_port_back_and_resuming_takes_it_again` is the test
that pins that behaviour, and it is the one that goes red first if gotatun
changes it.

**Two things that fall out of that window**, both found by driving this
against real NATs (issue #17):

- **A device mid-`remap` holds no socket at all**, for `PORT_RELEASE_MS`
  plus the measurement. Anything that finds a deployment *by its port* —
  `fuser -k`, a health check that probes `listen_port`, a supervisor that
  restarts what is not listening — will miss it and may conclude it is
  gone. Stop and check a deployment by pid.
- **A relayed device stays relayed across a `remap`.** The allocation
  lives on the transport factory, not on the socket pair `suspend` drops,
  so `resume` hands the rebuilt pair the same one. `remap` while relaying
  therefore measures the *direct* path, which is exactly what makes it
  useful there: it is how a program learns the relay is no longer needed.
  Held by the last act of
  `wireguard_traffic_can_fall_back_through_a_turn_allocation`.

**The queue is safe to declare late, and `report_ms` is a clock.** Both
were found the hard way (issue #15), by a rendezvous whose first line
after `queue.declare` was a wait for `wireguard_mapping`:

- Reports that are said **once** — the mapping, a roam, a relay taken or
  lost, a refusal — are held until a push lands, so the mapping is still
  there when a program declares its queue on its second line rather than
  its first. Only the `wireguard` snapshot is dropped on a refused push,
  because the next one carries the running totals. The hold is bounded;
  a queue that stays full is a sizing problem the deployment should see.
- `report_ms` ticks whether or not anything changed, including for a
  device with **no peers at all** — which is exactly the config a
  rendezvous starts from. A program can wait on the queue and get its
  interval. Changes still arrive early, ahead of the timer.

### The examples

`examples/21-wireguard` is everything that needs no privilege — `drt wg
keygen`, the two mirrored peer configs, and the refusals a config earns.
`examples/22-wireguard-interface` is the other half: it brings the interface
up and reads `/sys` to show the kernel really made it with the MTU the config
named, and that it goes away with the process. That one is marked
`needs_privilege` and the gate skips it without `--privileged`, loudly,
because a skip is never a pass.

Neither carries traffic between two peers, and `22`'s README says why: two
WireGuard interfaces on one host have local addresses at both ends, so the
kernel routes between them directly and nothing enters the tunnel. The
packet-crossing is proven in `crates/drt/tests/wireguard.rs` instead, where
the IP side is a pair of channels and no privilege is needed at all.

### When the punch cannot work: the TURN fallback

`punchable: false` is not the end of the road, it is a fork in it. A peer
behind a symmetric NAT can take a **TURN allocation** and publish *that*
address to the rendezvous instead of the measured one. Its WireGuard traffic
then goes through the relay, and the far side never learns the difference:
it has an endpoint, and packets arrive from it.

The program decides, because the program is what read `punchable: false`:

```lua
if msg.event == "wireguard_mapping" and not msg.punchable then
  -- The credential is crypto/turn_credential's; the wireguard block
  -- never holds a TURN secret.
  local c = host.call("crypto/turn_credential", { user = me, ttl = 3600 })
  queue.push(wg_out, {
    command  = "relay",
    server   = "203.0.113.5:3478",
    username = c.username,
    password = c.password,
  })
end

-- What comes back is the address to publish, in place of the measured one.
if msg.event == "wireguard_relay" then rendezvous_publish(msg.address) end
```

`{command = "relay", clear = true}` gives the allocation up again, so a
deployment that later gets a direct path can stop paying for the relay.

**A wrong or expired credential is refused with the wrong code**, and it
is not DRT's to fix: the `turn` crate answers 400 where RFC 8489 §9.2.4
says 401 with a fresh NONCE, so a browser's ICE agent — which retries a
401 and gives up on a 400 — cannot recover from a credential that lapsed.
`doc/TURN-401-Upstream.md` is the brief, and
`a_forged_credential_is_refused_with_the_wrong_code_for_now` is the
tripwire that goes red when upstream fixes it.

**The allocation does not outlive the credential, and nothing warns you.**
The `relay` command carries one username and password, and the refresh
loop inside the TURN client refreshes with that same pair. A
`<expiry>:<user>` credential stops verifying at its expiry, so the first
refresh after that is refused and the allocation — and the tunnel through
it — goes with it. A program that wants a relayed session longer than its
credential's TTL should mint a longer one up front. This follows from the
design and has **not** been measured: the lab run lasted minutes and the
TTL was an hour. Issue #17 §3 tracks making it survivable rather than
merely documented.

**What it costs, and the one flag.** `turn_fallback = true` on the block is
what makes a device able to do this at all, and it is off by default because
it is not free: a device that may relay gives up gotatun's batched
`recvmmsg` read, since a batch parked on the direct socket would starve the
relayed path. A device that will never relay should not pay for the option.

**A relay that fails must not take the tunnel with it.** gotatun's buffered
receive loop is `let Ok(()) = recv_many_from(..) else { return }` — one
error ends that task, silently and for good. A transport that propagated a
failed allocation would therefore kill a device whose direct socket was fine
all along, over a TURN server that restarted. So the transport never
propagates one: it drops the allocation, says so on stderr, keeps reading
the socket, and the drive loop tells the deployment (`wireguard_error`) so
the program can allocate again. The test drives the same path through
`clear` and then sends a packet directly, because both go through the same
drop and the same wake.

**Both paths stay live.** With an allocation installed, sends go through it
and receives listen on the direct socket *and* the relay. That is ICE's own
shape — the direct path is not torn down when a relayed one appears, so a
peer that later becomes reachable directly is still heard. It is also what
the first version got wrong: a receiver parked on the direct socket before
the allocation existed waited there forever while every packet arrived on
the path it was not watching, which showed up as a handshake the far side
answered and this one never heard. The allocation is a `watch` now, so
installing one wakes the receiver.

`wireguard_traffic_can_fall_back_through_a_turn_allocation` proves it end to
end against DRT's own `drt turn` server: an allocation taken with a
`crypto/turn_credential`-shaped credential, a real WireGuard handshake
across it, and a real packet out the far side. Loopback and unprivileged, so
— as everywhere else here — it proves the plumbing, not that it beats a real
symmetric NAT.

### The punch, measured — elsewhere, and in a lab

This section used to say a punch would be proven by "two hosts behind two
different NATs, a rendezvous between them, and a packet across with no
relay in the path", and that until someone ran it the punch was
*plausible on well-understood grounds* rather than measured.

**Someone ran it.** Not here: discofetch drove its own rendezvous program
(`deploy/tunnel/wg-rendezvous.dlua`) against this block, with two hosts in
network namespaces behind two netfilter MASQUERADE NATs and a router with
a default-drop firewall. Recorded in issue #15, scoped in issue #17:

- **Direct, through two NATs, no relay in the path.** Handshakes at 247 ms
  and 1747 ms, packets across both ways.
- **Past a symmetric NAT** (`MASQUERADE --random-fully`), with
  `turn_fallback = true` and a `drt turn` on the segment: mapping
  classified symmetric from two disagreeing STUN answers, allocation
  taken, handshake at 739 ms, packet across.

That is step 4 of the ladder above, measured, and the TURN fallback
measured as the thing it exists for. It also found two bugs that no
loopback test could have — the dropped one-shot reports and the peerless
device that never ticked — which is the strongest argument that the run
was worth more than the tests here.

**What it still does not settle.** The NATs were netfilter on one
machine, not consumer or carrier-grade hardware on the open internet, so
it says nothing about NAT implementations DRT has not seen, real RTTs,
or the middleboxes between two houses. The conntrack finding in step 4 is
exactly the kind of thing that varies by device. Treat the punch as
**measured against Linux NATs in a lab, and plausible on well-understood
grounds everywhere else** — which is a materially stronger claim than
this document could make a day ago, and still not "it works on your
router".

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

- **A device built with no peers does not route.** Measured on gotatun
  0.9.2: build one with an empty peer list, add a peer at run time, send
  to its allowed IP, and nothing leaves; add any peer at build time and
  the identical runtime add works. That is exactly the config a
  rendezvous writes, so `apply` forces the connection to rebuild when the
  first peer arrives, and
  `a_peer_the_config_never_named_can_be_added_and_then_reached` holds the
  fix in place. Worth reporting upstream; the workaround costs nothing,
  since a device with no peers has no session to tear down.

- **No rendezvous.** §2 step 3 is a shape, not a service. The exchange is
  a program's, over the relay.
- **No route management, but no longer in silence.** DRT creates the
  interface, gives it the `address` and MTU the block names, and brings it
  up — so the common case (one subnet of peers, routed by the address's own
  prefix) works with no `ip` commands at all. A second address, or a route
  to something outside that prefix, is still `ip addr add` /
  `ip route add`.

  What changed is that a peer whose `allowed_ips` no route will reach is
  **named at startup**, with the command that fixes it. That case — a
  tunnel that comes up, reports a handshake, and silently carries nothing —
  is the one this gap actually produces, and both numbers are in the config,
  so it can be a sentence instead of an afternoon with tcpdump. A warning
  and not a refusal: a hub whose whole job is to reach subnets outside its
  own prefix is a legitimate config, and it works the moment the route
  exists.

  Real route management means netlink on Linux, `PF_ROUTE` on macOS and the
  IP Helper API on Windows — three implementations, which is why `wg-quick`
  is a per-platform shell script. Not started, and it should be a decision
  about whether DRT owns routes at all rather than a drive-by.
- **No no-root mode.** The device still wants a kernel interface, so a
  deployment needs `CAP_NET_ADMIN`. The pieces for a userspace mode are
  all present — the IP side is a trait, and the tests already drive it
  with channels — so a `--local`-shaped local port terminating TCP
  in-process over a userspace stack (smoltcp) is the next change, and it
  touches only the IP side of what is here. That is also §8's "reliable
  stream over the UDP hole" from `doc/Ask-Discofetch-Reply.md`, answered
  with WireGuard instead of QUIC.
- **Not on Windows yet.** The cross-build works, but `full` does not
  build for Windows for unrelated reasons (`exec` is unix-only,
  aws-lc-sys through russh), and `wintun.dll` would have to ship beside
  the binary. `doc/Platforms.md` has the state of that.
- **Not in `wasi` or `web`.** Neither has a tunnel interface, and the
  wasm targets have no threads to drive one.
