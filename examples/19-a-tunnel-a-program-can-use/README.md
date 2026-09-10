# 19-a-tunnel-a-program-can-use

The relay, end to end, on one machine — and the half that was missing until
now: a **local port** on the caller's side, so a program reaches a device
that has no inbound address.

## Run it

Three terminals, or `./demo.sh` which is all of them:

```
cd examples/19-a-tunnel-a-program-can-use
drt relay --config rendezvous.host.lua
drt start --config device.json
drt --config park.json tunnel
drt --config claim.json tunnel
```

The two tunnel files are the flags under the block's names -- `park.json`
is `--park … --to 127.0.0.1:18491`, `claim.json` is the claim URL with
`--local 127.0.0.1:18492` as `bind` -- and either command takes the flags
instead:

```
drt tunnel --park "ws://127.0.0.1:18490/park/fp?k=park-key-for-the-example-only" \
           --to 127.0.0.1:18491
drt tunnel "ws://127.0.0.1:18490/s/fp?k=caller-key-for-the-example-only" \
           --local 127.0.0.1:18492
```

Then anything that speaks TCP:

```
curl http://127.0.0.1:18492/hello
```

## What you should see

```
answered through the tunnel: /hello
answered through the tunnel: /again
```

Two requests, two separate legs. The relay carries bytes and reads none of
them.

## What it teaches

**`--local` is the half `ssh -o ProxyCommand` could not give a program.**
The caller side of a tunnel used to be stdio only, which suits `ssh` and
nothing else: a connector dials a `host:port` and cannot hand its stdio to a
subprocess. `--local` binds a port instead, so `ssh/exec` scoped to
`127.0.0.1:18492`, `rest` pointed at it, or a desktop client with no
ProxyCommand support all reach the device. `--listen` is the *other* half —
it serves the device side — which is why this is not spelled with it.

**One connection, one leg.** Each accepted connection opens its own WSS
connection and claims its own parked leg; nothing is multiplexed over one.
That is what keeps the relay's per-leg accounting true, and it is why the
device re-parks the moment a leg is claimed. Both curls above went through
legs of their own.

**A refused claim closes the local connection, and does not hang it.** A
wrong caller key or an unknown label is a 403 at upgrade time, and the
accepted socket is dropped at once rather than left half-open for a client to
sit on. Change one character of `caller_key` above and `curl` fails
immediately instead of waiting.

**The program does not know any of this is happening.** `app.dlua` and
`device.json` are `17-serving-http`'s listener, unchanged. Reachability is
the deployment's problem; the program declares a queue and answers what
arrives. Move the device behind CGNAT and nothing in it changes.

**The keys are per label, and there are two.** The park key lives on the
device forever; the caller key is what you hand out. Either can be rotated
without the other, which is what per-label revocation is for. These are
fixed strings so the example is reproducible — real ones come from
`openssl rand -hex 24`.

**The key is in a file, not in `ps`.** The `?k=` in a park or claim URL is
a credential, and on a command line it is in the process list, in shell
history, and in every "run this" someone pastes. `park.json` and
`claim.json` are the same two commands with the key in a file a setup
script writes and a unit runs; `chmod 600` them. A flag typed beside the
file replaces the key it names, and a flag naming a different mode than
the file is refused as the conflict it is, so a file cannot be quietly
overridden into the wrong half.

`11-tunnel-and-relay` is the same machinery with the refusals shown, and
`14-ssh-through-a-tunnel` is the ProxyCommand form for a person at a shell.
