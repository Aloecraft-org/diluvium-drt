# 13-stun-server

Running the thing `09-netcheck` asks questions of. Two STUN binding servers,
and a client that classifies this machine's NAT from what they answer.

## Run it

Two servers, each in its own terminal:

```
drt stun --config stun1.json
drt stun --config stun2.json
```

Then, in a third:

```
drt netcheck --stun 127.0.0.1:34780 --stun 127.0.0.1:34781
```

`./demo.sh` does all three unattended, which is how the gate runs it.

## What you should see

```
punchable — the UDP mapping is endpoint-independent, so the address a STUN server sees is the address a peer can reach
  use: use a rendezvous; peers will connect to you directly

evidence
  udp map    <port> (127.0.0.1:34780), <port> (127.0.0.1:34781)  independent
```

Both servers report the **same** mapped port, so the mapping does not change
with the destination, so a peer can reach you at the address a STUN server
saw. That is what `punchable` means.

## What it teaches

**Two servers, not one.** A single server tells you the address it saw. It
takes a second one, on a different address, to tell you whether that address
*changed between vantage points* — and that is the fact that decides whether
hole punching can work. `detect_mapping` refuses below two rather than
guessing, so one server yields "not measured", never a confident wrong
answer.

**The config says where to bind, and there is no safe default.** The relay
and the http listener default to loopback, because an edge terminates TLS in
front of them. A STUN server is the opposite: reporting the address the world
sees is the entire service, so it is meant to face the world and the config
has to say so out loud. Loopback here is only because this example probes
itself.

**A server is an app like any other.** `stun1.json` is a config with one
block and no program — `drt stun` reads it and serves. Inside `drt start` the
same server also reports its counters to the root program, on the queue the
`stun` block names.

Put it behind the same rate limiting as anything else that answers strangers.
