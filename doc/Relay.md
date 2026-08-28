# The relay: SSH to anything, from anywhere

A fetchpoint behind CGNAT has no inbound address. Hole-punching (STUN,
WebRTC) gets you a direct path where the NAT topology allows one; where it
does not — address-and-port-dependent filtering, carrier-grade NAT — the
carrier that always works is an **outbound WSS connection to something
reachable**. That is what the relay is: a rendezvous point where a device's
outbound leg and a caller's outbound leg are spliced into one byte pipe.

SSH does not care what carries it. So the whole design is a pipe that never
looks inside, and everything you already know about ssh keeps working
through it: `rsync`, `sftp`, `-L`/`-R` tunnels, agent forwarding,
host-key verification, auth. Those stay end-to-end between your ssh client
and the sshd. A compromised relay can drop your connection; it reads only
ciphertext.

```text
   device (behind CGNAT)          relay (public)            caller
   drt tunnel --park … --to  ──►  drt relay          ◄──  drt tunnel wss://…
        │                          splice, by label            │
     127.0.0.1:22                                          ssh/rsync/sftp
```

## The three corners

### 1. The relay

A `relay` block in the config, then `drt relay` (standalone) or `drt start`
(inside a deployment — see *The control plane* below).

```lua
-- rendezvous.host.lua
return {
  supervisor = "supervisor.lua",
  relay = {
    bind = "0.0.0.0",
    port = 8443,
    labels = {
      xps = { park_key   = "…32+ random bytes…",
              caller_key = "…different 32+ random bytes…" },
    },
  },
}
```

```
drt --config rendezvous.host.lua relay
```

Two keys per label, deliberately: the device's key admits a leg to be
*parked*, the caller's key admits one to *claim*. They are different
secrets because they are held by different parties — the device key lives
on the device forever, the caller key is what you hand someone who should
be able to reach it. A label with either key missing is a config error at
load, not a silent refusal at connect time.

Put it behind your existing TLS terminator (`wss://` on 443 is what
middleboxes let through); the relay itself speaks plain WebSocket.

### 2. The device

One long-running process on the machine you want to reach:

```
drt tunnel --park wss://rendezvous.example/park/xps?k=$PARK_KEY --to 127.0.0.1:22
```

It holds a parked leg open, answering the relay's pings so the NAT mapping
stays warm. When a caller claims it, the device dials `127.0.0.1:22`
*lazily* — the local sshd sees a connection only when someone is actually
calling — and immediately parks a fresh leg. That is **replenish-on-claim**,
and it is the entire concurrency story: N concurrent sessions need no
control protocol, only N claims and N replenishes. It reconnects forever
with backoff, so a relay restart or a lost uplink heals itself.

### 3. The caller

The OpenSSH `ProxyCommand` contract — bytes on stdin/stdout, nothing more:

```
ssh -o ProxyCommand="drt tunnel wss://rendezvous.example/s/xps?k=$CALLER_KEY" user@xps

rsync -av -e 'ssh -o ProxyCommand="drt tunnel wss://rendezvous.example/s/xps?k=$CALLER_KEY"' \
      ./src/ user@xps:/srv/

sftp -o ProxyCommand="drt tunnel wss://rendezvous.example/s/xps?k=$CALLER_KEY" user@xps
```

Because the bridge only moves bytes, `-L 8080:localhost:8080`, `-R`, `-A`,
`ssh -J` and everything else are the real ssh client's and work unchanged.

## The URLs are the public surface

Both legs are dumb pipes — `websocat` on either end works identically, and
that is the compatibility promise. So the versioned surface is the URL
shape, the HTTP status, and the raw bytes. Nothing else:

```text
  park a leg:    wss://<host>/park/<label>?k=<park_key>
  claim a leg:   wss://<host>/s/<label>?k=<caller_key>
```

- **403** at the handshake: bad key, or unknown label — deliberately
  indistinguishable, so probing the relay tells you nothing. The WebSocket
  never exists, which is what keeps a public relay from being an open proxy.
- **Close 1013**, "the device is not home": right key, nothing parked.
  A clean disconnect, not a hang, so a caller can retry or report.
- **Close 1008**, "refused by the deployment": arbitration said no.

A claim manifests as **the first caller byte** — there is no control
message, because a dumb pipe cannot read one. An ssh client's banner is
the claim.

## The control plane

Standalone, the relay tells nobody anything and the static per-label key is
the only gate. Run inside `drt start`, it gets a channel to the root
program, and three things become true at once — they were never three
features, only one missing channel:

```lua
relay = {
  bind = "0.0.0.0", port = 8443,
  queue = "relay_in",        -- events land here (default)
  reply_queue = "relay_out", -- answers read here; ABSENT MEANS NO ARBITRATION
  admit_timeout_ms = 2000,
  labels = { … },
}
```

**Presence.** `parked` and `claimed` arrive on `relay_in` as ordinary
msgpack messages. A panel can say *the laptop is home* without asking
anything.

**Metering.** `closed` carries the session's total bytes, both directions
together — the number a meter bills or throttles on. The relay counts; the
deployment decides what that means.

**Arbitration.** If — and only if — you name a `reply_queue`, the relay
asks `admit` before a leg proceeds, and your answer decides. This is the
thing static keys cannot express: a per-tenant quota, a maintenance window,
a revoked device, a rate limit.

The messages, flat maps tagged by `event` (the same shape the http listener
uses, so a program branches on a field and never on position):

| `event` | fields |
|---|---|
| `parked` | `label` |
| `claimed` | `label`, `session` |
| `closed` | `label`, `session`, `bytes` |
| `admit` | `tok`, `label`, `leg` (`"park"` or `"caller"`) |

An `admit` question is answered by pushing `{tok = …, ok = true｜false}` to
the reply queue, naming the token you were asked with — the same token
discipline as every other hostcall reply in DRT.

```lua
local ev  = queue.declare('relay_in',  {capacity = 256})
local out = queue.declare('relay_out', {capacity = 256})

while true do
  local _, m = queue.wait({ev})
  if m.event == 'admit' then
    queue.push(out, {tok = m.tok, ok = allowed(m.label, m.leg)})
  elseif m.event == 'closed' then
    bill(m.label, m.bytes)
  elseif m.event == 'parked' then
    mark_home(m.label)
  end
end
```

### Two rules worth internalizing

**Arbitration is opt-in, and opting in is opting into answering.** An
absent `reply_queue` means the key stays the only gate — the default, and
identical to standalone behavior. Once you have named one, a question you
do not answer within `admit_timeout_ms` is a **refusal**. Silence must fail
closed for exactly the reason an empty key refuses every leg: the regret
asymmetry is total.

**The key is checked before the upgrade; policy after it.** An
unauthorized leg never becomes a WebSocket (403, cheap, structural). The
deployment's policy is a second, slower question asked only of connections
that already passed the first. A handshake callback cannot be async, and it
should not be.

## What is deliberately not here

- **Tickets.** Today it is a static per-label key from config, compared in
  constant time. HMAC tickets (`{label, leg, expires}`) replace the key
  *values* later, not this shape — `verify_key` is the one function that
  changes, and no URL a user types changes with it.
- **A control channel on the wire.** Parked-pool is the subset a dumb
  client speaks forever. If richer per-session control is ever needed it
  goes *beside* `/park`, never in place of it.
