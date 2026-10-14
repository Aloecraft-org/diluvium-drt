# 09-netcheck

`drt netcheck <location>` asks a reflect server what it and its peer gate
see of you, and prints the answers with no verdict. It is
`drt p2p drt+reflect://<location>`.

## Run it

Two gates on this machine, then the question:

```
cd examples/09-netcheck
export REFLECT_KEY=demo
drt p2p --reflect 34790 --reflect-peer 127.0.0.1:34791 --reflect-key env:REFLECT_KEY &
drt p2p --reflect 34791 --reflect-peer 127.0.0.1:34790 --reflect-key env:REFLECT_KEY &
drt netcheck 127.0.0.1:34790
# example: omits --json and stopping the gates; demo.sh does both of the latter.
```

## What you should see

```
location   drt+reflect://127.0.0.1:34790
server     127.0.0.1:34790
offers     udp,tcp,peer,filtering,cross
other      127.0.0.1:34791
udp        ok mapped 127.0.0.1:40434 local 127.0.0.1:40434 0 ms
mapping    ok none (other gate saw 127.0.0.1:40434)
filtering  same_address
tcp        ok mapped 127.0.0.1:60690 local 127.0.0.1:60690 0 ms
cross      port 44975 connected token received
```

## What it teaches

**One location is enough.** The first answer names the other gate
(`other`), and the checks that need it go there from the same socket.

**A mapping is a comparison.** One socket asks both gates. The same mapped
port at both is `endpoint_independent`; a port per destination is
`endpoint_dependent`. On loopback there is no NAT, so it is `none`.

**Every line has a code.** `filtering` needs the peer gate to answer from
another address, and here both gates share one, so it says `same_address`
instead of guessing. A server with no peer says `no_peer`.

**`cross` is the inbound test.** This side listens on a port, the other gate
connects to the address the server saw, and the token it writes proves who
arrived. Behind a NAT that port answers `timeout` or `refused`.

The codes are in `doc/Reflect.md`.
