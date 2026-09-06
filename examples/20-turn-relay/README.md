# 20-turn-relay

The last rung of the traversal ladder, and the credential a program mints
for it without ever holding the secret.

`09-netcheck` says whether a direct path can exist. `13-stun-server` runs the
servers that measure the mapping deciding it. When the answer is no — a
symmetric NAT, a browser with no UDP — something has to carry the traffic.
This is that something, and it costs bandwidth, which is why it is last.

## Run it

Two terminals, or `./demo.sh` which is both:

```
cd examples/20-turn-relay
drt turn --config turn.host.lua
drt run --config app.host.lua
```

## What you should see

```
$ drt turn --config turn.host.lua
drt turn: listening on 127.0.0.1:18493, relaying via 127.0.0.1

$ drt run --config app.host.lua
username <expiry>:fp-7
password <hmac-sha1 of the username, base64>
expires  <now + ttl>
uri      turn:127.0.0.1:18493?transport=udp

$ drt turn --config open.host.lua
drt turn: turn: the key is missing or shorter than 16 bytes …; a relay with
nothing to verify against is an open relay, and is refused
```

## What this example does not show

**It does not relay any traffic.** Proving that needs a TURN client, and DRT
ships none — `drt netcheck` is a STUN client, which is why `13-stun-server`
can prove its server end to end and this cannot. The gate that does prove it
is `crates/drt/tests/turn.rs`, which allocates through the server with
webrtc-rs's client, relays bytes to a far peer, and checks the closing byte
count reaches the supervisor with its principal.

What this example shows is the half a deployment writes: the block, the
credential, and the two refusals.

## What it teaches

**One secret, two blocks, and that is the whole deployment.**
`turn.host.lua` verifies what `app.host.lua`'s `connectors.crypto.turn`
mints. Nothing is registered anywhere first, and no database sits between
them: the username carries its own expiry in cleartext and the password is
that username's HMAC-SHA1 under the shared secret, so any server holding the
secret can check it. That is coturn's `use-auth-secret` scheme byte for
byte, so the same credential works against coturn and this works against a
credential coturn minted.

**The guest never holds the secret.** `app.dlua` asks for a credential; it
cannot read the key that signs one, exactly as with `crypto/jwt_sign`. It
also cannot choose the expiry — it passes a `ttl` and the host adds it to
its own clock, because the expiry is one field of a cleartext username and a
guest that chose it would be one edit from a credential that never expires.

**An open relay is refused, not warned about.** `open.host.lua` is the same
block with the key removed. A TURN server with nothing to verify against
relays for anyone who finds it, so it refuses to bind rather than starting
and hoping nobody scans it.

**The URIs are echoed, not invented.** `uris` comes from the config and
comes back in the reply, so a program hands a peer a complete ICE server
entry without hard-coding where the relay lives — and moving the relay is a
config edit, not a redeploy.

Inside `drt start` the same server also reports its counters and, as each
allocation closes, the bytes it relayed and the principal it belonged to —
which is what makes relay bandwidth attributable to a name.
