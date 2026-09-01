# 09-netcheck

`drt netcheck` answers one question — what will this network carry — with one
of four verdicts: `direct`, `v6-direct`, `punchable`, `relay`. Each comes with
a sentence of advice and the measurements that produced it.

## Run it

```sh
cd examples/09-netcheck
drt netcheck
drt netcheck --stun stun.l.google.com:19302
```

## What you should see

```
$ drt netcheck
relay — the UDP mapping could not be measured, and relay is the answer that works on every network
  use: use a tunnel

evidence
  address    not measured (no reflect edge answered)
  v6         none routable
  udp map    not measured
  tcp map    not measured
  inbound    not tested (no --port given)
exit 1

$ drt netcheck --stun stun.l.google.com:19302
   ... the same nine lines, then exit 1
```

Neither run sent a packet; the second returns in milliseconds, refused before
a socket is opened. The `v6` line is read from your routing table rather than
from the network, so a machine holding a routable IPv6 address prints it here,
answers `v6-direct`, and exits 0.

## What it teaches

**A mapping is a comparison, not a reading.** One socket, asked of two STUN
servers on separate addresses. The same port at both is endpoint-independent,
and the address a STUN server sees is then one a peer can reach; a fresh port
per destination is symmetric, and what a STUN server sees says nothing about
what a peer would see. One server compares against nothing, so netcheck
reports `not measured` instead of guessing.

**The UDP mapping decides; the TCP line is context.** A NAT can be
endpoint-independent for TCP and symmetric for UDP, and it is the UDP
behaviour a hole punch lands on. A verdict built on `tcp map` would be
confidently wrong on exactly the networks where being right matters.

**`not measured` is a finding, so it gets a line.** A silently missing one
reads as a measurement that passed. `address`, `tcp map` and `inbound` are an
edge's half of the work and v0.4.0 has no edge to ask, so they say the above
on every network — and there is no `--port` to give.

**`relay` is the fallback, and the exit code separates the two.** Taking relay
on a network that could have punched costs a hop; the opposite mistake costs a
connection that never forms. A measured verdict exits 0, `relay` included; the
`exit 1` above is nothing measured at all.

## With servers of your own

```sh
drt netcheck --stun stun.l.google.com:19302 --stun stun.cloudflare.com:3478
# example: omits --json and the case over the verdict a deploy script writes.
```

Two names that resolve to one host are one destination, and one destination
looks endpoint-independent under any NAT — which is why the flag asks for two
on separate addresses.
