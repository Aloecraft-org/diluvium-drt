# 32-kv

A key-value store on the signalling server. `drt p2p --match` with `--kv`
keeps one, in SQLite, at `/v1/<topic>/kv/<key>` over plain HTTP, so `drt
kv`, `curl`, a page and a program all reach the same keys.

## Run it

```
cd examples/32-kv
drt p2p --match 18497 --kv ./data
```

and in another terminal:

```
K=drt://127.0.0.1:18497/v1/home/kv
drt kv put $K/bpi/info "fingerprint=SHA256:..."
drt kv put --if-version 7 $K/bpi/info "someone else"
curl -s http://127.0.0.1:18497/v1/home/kv/bpi/info
drt run --config app.json
drt kv ls $K
```

`./demo.sh` runs all of it, which is how the gate runs it.

## What you should see

```
1
drt kv: the key is at version 1, not that one
fingerprint=SHA256:...
visits	1
visits	2
present	presence/pi	up	30s
bpi/info	v1
presence/pi	v4, expires in 30s
visits	v3
```

## What it teaches

**Every key has a version, and a write can name the one it replaces.**
`--if-version 7` is `If-Match: "7"` on the wire; the key is at 1, so the
write is refused and says where the key is. `app.dlua`'s `increment` is
the same idea as a loop: read, add one, write over the version read, and
read again if someone else won. Version 0 means "only if absent".

**A TTL key lives only while it is written again in time.** That is
presence: a peer that stops refreshing `presence/pi` drops out of the
listing, and a watcher (`drt kv watch`) is told it expired.

**The program names keys; the deployment names the topic.** `app.json`
wires the `kv` connector to one topic URL, so the program cannot reach
another topic or server. This server has no `--topics`, so it takes no
tokens: whoever reaches the port may read and write, and `--host` stays
at its loopback default. With `--topics`, the connector's `token` is a
`read`/`write` token of the topic.
`doc/DRT-Signalling.md` §10 is the whole protocol.
