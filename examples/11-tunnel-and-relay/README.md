# 11-tunnel-and-relay

A machine behind CGNAT has no inbound address, so both ends dial out and meet
in the middle. The relay is a public address holding parked WebSocket legs,
splicing the two that carry the same label into one byte pipe. ssh rides over
it unchanged, because the pipe never looks inside.

```text
  device (no inbound address)      relay (public)              you
  drt tunnel --park … --to  ──►    drt relay    ◄──   drt tunnel wss://…
       │                        spliced by label               │
   127.0.0.1:22                                        ssh, rsync, sftp
```

## Run it

```
cd examples/11-tunnel-and-relay
drt tunnel
drt relay --config rendezvous.host.lua
```

The rest wants two machines and a public name, so these are the two lines that
run here. Neither opens a port.

## What you should see

```
$ drt tunnel
drt tunnel: name a URL to bridge stdio to (with --local to serve a local port instead), --listen with --to, or --park with --to; or `tunnel` in the --config file, which takes the same keys
exit 1

$ drt relay --config rendezvous.host.lua
drt: rendezvous.host.lua: relay.labels.xps needs both park_key and caller_key; an absent key refuses every leg
exit 1
```

## What it teaches

**One command, three shapes.** A URL to bridge stdio to, `--listen`/`--to` in
front of an sshd you can already reach, `--park`/`--to` on one you cannot.
Given none of them, `drt tunnel` names all three rather than guessing.

**Or one file.** Every flag is a key of a `tunnel` block, so the device's
half is a file a setup script writes and a unit runs, with the key out of
`ps` and shell history:

```json
{ "tunnel": { "park": "wss://rendezvous.example/park/xps?k=…", "to": "127.0.0.1:22" } }
```

```
drt --config park.json tunnel
```

The URL is `claim` and `--local` is `bind`; `listen`, `to` and `park` are
spelled as the flags are. A flag typed beside the file replaces the key it
names, and a flag that names a different mode than the file is refused as
the conflict it is. `19-a-tunnel-a-program-can-use` runs both halves that
way.

**The relay is configured, not flagged.** It reads the `relay` block of a
config; its keys ship blank, and a blank key is refused when the file loads
rather than at connect time — a refusal is a reply, not an exception, and this
one arrives before anything is listening. Fill in two per label with `openssl
rand -hex 24`, never the same value for both: the park key lives on the device
forever, the caller key is the one you hand out.

**The key is in the URL**, so it is in your shell history and in any proxy log
in front of the relay. That is the price of a leg a dumb `websocat` can speak,
and it is why the keys are per label and rotatable. Terminate TLS in front of
the relay too — it speaks plain WebSocket, so put it behind whatever already
answers 443 for you.

## The three commands

The relay on a public machine, the device holding a leg open, and you:

```
drt relay --config rendezvous.host.lua
drt tunnel --park "wss://rendezvous.example/park/xps?k=$PARK_KEY" --to 127.0.0.1:22
ssh -o ProxyCommand="drt tunnel wss://rendezvous.example/s/xps?k=$CALLER_KEY" user@xps
```

`rsync -e`, `sftp -o`, `-L`/`-R` and agent forwarding all work through that
same ProxyCommand, because the bridge moves bytes and nothing else. The device
dials `127.0.0.1:22` only once someone calls, and re-parks the moment a leg is
claimed, so a second caller needs no coordination.
