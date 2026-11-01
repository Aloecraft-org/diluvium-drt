# 0.7.0: ownership, and the four things discofetch is blocked on

**Status:** the plan of record for the release after `0.6.2`, and the
document to review. The version number is provisional until the first
change lands, per `doc/Plan-2026-09.md`. Nothing here is built. Where a
claim rests on code it names the file, and where it rests on a trace into
diluvium it says so.

It was `doc/Ownership.md` until the release grew past that name; the git
history is continuous through the rename.

## 0. What ships

| # | what | why it is here |
|---|---|---|
| §2 | **The ownership dimension** — a host-held resource belongs to a node or to the root, and is released when its owner dies | the spine; everything below is a consumer of it |
| §3 | **Node-owned sockets**, stream and datagram | one faulty client bounded to one node; parked services that hibernate. Datagram ships for ownership parity, **not** for the per-room relay port (§3.1) |
| §4 | **`crypto/derive { label }`** — a runtime name for a host-held subkey, with a prefix scope and per-consumer subkeys | a room made at 14:02 cannot have a config entry |
| §5 | **`kid` in the JWT header**, algorithm from the named key, interop secrets refused | cheap now, a token-format break in a year |
| §6 | **The park deadline outlives residency** | nothing fires on bytes *not* arriving |
| §7 | Plugins: **deferred**, design settled | the dispatcher change does not belong in an expedited release |
| §8 | Inter-root: **prerequisites only** | its blocker is a document, not code |

One idea holds it together: **a host-held resource can belong to a node
rather than to the root.** Sockets are its first consumer and the one that
exercises it hardest. §4's labels are its second, for free. Plugins would
be its third and are the reason §7 is deferred rather than dropped.

---

## 1. What is missing today

A connector may already hold live OS state on a guest's behalf — the
`Connector::finish` doc says why, and it is not a hypothetical. `sql`
caches handles across calls so a guest can `BEGIN` in one hostcall and
`COMMIT` in another; SQLite rolls back silently on a dropped connection,
with the writes gone and the exit status still zero, and every layer above
believed the `ok` it was given. The trait grew `finish` so a connector that
can lose work at teardown is able to say so.

So the hard half is conceded. What does not exist is *whose*:

- **A connector never learns the caller.** `Connector::call(&self, call,
  args, scope)` — caps gate the call before it arrives, but no identity
  comes with it (`crates/drt-connector/src/lib.rs`).
- **`sql` keys its cache by `PathBuf`** (`connectors/sql/src/lib.rs`), so
  handles are root-wide. Two nodes that can name one database share one
  connection, and therefore one transaction.
- **`finish` fires once**, across every wired connector, when the root goes
  (`crates/drt/src/run.rs:178`). `do_kill` does not touch connectors.
  There is no per-instance release path at all.

Nothing above is a bug being fixed. It is a dimension that was never
needed, because until now every resource a connector held was one the
whole root shared.

## 2. The ownership dimension

### 2.1 A caller has a name

`Connector::call` learns which instance is calling. **Additive**: a new
trait method carrying the caller, defaulting to the existing one, so the
nine connectors in `connectors/` compile untouched and only those that
care implement it.

The caller is passed as a struct rather than a bare id, so that adding the
node's path later is not a second signature change.

### 2.2 A handle belongs to exactly one instance

A handle is opaque to the guest and looked up by `(owner, handle)`. A node
presenting a number it did not receive gets **no such handle** — not a
permission failure, because from where it stands the handle does not
exist. A denial would tell it something true about another node.

### 2.3 There are two lifetimes, so the scope is two-valued

A host-held resource can follow the root's lifetime or a node's. There is
no third, so the ownership dimension is not a flag bolted onto one feature
— it is the shape the runtime already has, written down.

- **`root`** — one instance for the root, released when the root goes.
  This is what every connector does today, unstated.
- **`node`** — one instance per calling node, released when that node dies.

The instance table is keyed by the scope's key: the owner for `node`, one
root key for `root`. Release-by-owner then falls out — a root-scoped
instance matches no owner and survives to teardown without a second path.

**Supporting both costs less than supporting one.** Keyed this way they
are one mechanism; pick `node` alone and `root` becomes a special case
that has to be added back the first time something genuinely is a
singleton. It is also why `sql`'s current behaviour is not a wart to be
removed but a scope to be *stated*: it is `root`, and saying so is the fix
(§11, risk 3).

### 2.4 Death releases; hibernation does not

`release(owner)` runs on the instance-death path — kill, budget
exhaustion, a trap. It closes that node's handles and nothing else's.

**Hibernation must not release.** A parked service holding a quiet socket
for hours is the case this release exists to serve; a handle that did not
survive hibernation would force every parked service to stay resident,
which is the cost the whole design is trying to avoid.

### 2.5 A release says what was lost

`finish`'s lesson applies per node: releasing must be able to report what
did not survive, attributed to the node that owned it. "Nothing was lost"
is a claim, not a shrug — the wording `finish` already uses.

## 3. Node-owned sockets

### 3.1 What a node can hold: two verb sets, one mechanism

**Stream** — listen, accept, read, write, close.
**Datagram** — bind, recv, send, close. No accept, because there is
nothing to accept.

Both are capability-gated with a scope naming what may be bound, the shape
`connectors/exec`'s `allow` already establishes for "which of these may a
guest name".

**UDP is in, and the reason is parity rather than a launch consumer.**
The ownership dimension is identical for a datagram socket — bind, own,
release, survive hibernation — and only the vocabulary differs, so leaving
datagrams out means a second pass through the same connector to add four
verbs later.

**It does not ship for the per-room relay port, and an earlier draft said
otherwise.** That draft justified UDP by a consumer described as *"the
fetchpoint allocates one UDP port per room and forwards everything on it
down the tunnel to the hub"* — and then committed, three paragraphs later,
that the guest-shuffled path is not the relay. Forwarding tunnel traffic
**is** data plane, so those two sentences could not both be true. The
commitment is the one that stands: that port is a next-release path
needing the node-scoped block, not a 0.7.0 one.

What is left for the guest datagram path at 0.7.0 is control-plane
traffic — per-room keepalives, handshakes, anything at signalling rates.
**If there is no such consumer at launch, UDP can slip to the next release
alongside the relay block at no cost to anything else here**, and that is
a cheaper decision to make now than after the verbs exist. Stated plainly
so it is a choice rather than an assumption nobody noticed making.

**The existing transport blocks stay root-wide, and that is a fork worth
naming.** `stun::bind`, `turn::bind`, the relay's `TcpListener::bind` and
`listen::bind` all bind from config at start
(`crates/drt/src/start.rs:354`). A per-room UDP port can therefore be had
two ways: a room node binds its own datagram handle and shuffles the bytes
itself, which is simple and puts every packet through a guest; or one of
those blocks gains `node` scope (§2.3) and forwards host-side with a
node-owned lifetime. **This is a sequencing commitment, not a menu.** The
guest-shuffled path is for control-plane datagrams and for the demo. It is
**not the relay**, and it must not become the relay by being the one that
shipped first and worked: a data plane that moves every WireGuard packet
through a guest is a different thing wearing the same shape. The
node-scoped relay block is the next release's headline, and until it
exists the guest path carries that sentence in its own documentation.

**A sentence will not hold that line on its own, so it ships with a
number.** The guest path's measured packets-per-second ceiling goes in its
own documentation, so "this is not the relay" is checkable rather than
promised. A control-plane port at launch rates is far under it; a relay
needs orders of magnitude more, and a measured ceiling makes that
arithmetic rather than judgement.

What UDP does **not** bring with it is the per-client isolation of §3.7.
There are no connections, so there is no node per connection: one node
owns the port and demultiplexes packets itself. Datagram sockets get
ownership and lifetime; they do not get blast radius. Anyone expecting a
faulty sender to be contained the way a faulty TCP client is has read too
much into this section.

A guest never holds a descriptor; it cannot, it has no syscalls. It holds
a handle and the host holds the fd. "Node-owned" is a statement about
lifetime and reach, not about who calls `read`.

### 3.2 Accept spawns, and the handle transfers

The pattern this release exists for:

```
    a node accepts  ->  spawns a child  ->  transfers the connection to it
```

Transfer is an **explicit verb**, not a side effect of spawning. Per-
instance ownership otherwise forbids exactly this, and a rule with a
silent exception is worse than a rule with a named one. It is the only
genuinely new API surface here, and the place to expect argument.

**Where the acceptor lives is a separate question, and the answer is: not
in the thing being served.** One acceptor per root spawns a child per
connection; the child reads whatever the far end says it wants and then
attaches itself to that subject — a room, a tenant, a session — **by queue
message**. The alternative, an acceptor per room, means every room binds
its own listener behind the proxy, which is a port per room for no gain.

The rule it rests on: **the ownership tree is not the membership graph.**
A child owns its socket because it was transferred one; it belongs to a
room because it said so and the room agreed. Parentage is about lifetime,
not membership.

That also makes "deleting a room ends its parked services" an explicit
message rather than a tree teardown — which is the right shape, because a
teardown that happens by structure cannot say what it lost, and §2.5 asks
that it be able to.

### 3.3 A vital handle, because the guarantee has a direction

§2.2 and §2.4 guarantee **node dies → handle closes**. They do not
guarantee the converse, and the consumer design wants the converse. Its
requirement, quoted because the document it comes from is a discofetch
draft that is not in either repository: *"socket closes, node exits,
service gone, nobody needs notifying."*

Without something here, a clean close is a readiness event (§3.4) that the
node observes and then *chooses* to exit on. That is program logic. A node
with a bug sits there holding a dead socket, and the service is gone while
the node is not.

So a handle may be declared **vital** — at creation, or at transfer (§3.2),
which is where a per-connection child gets one. The contract is: *this node
exists to serve this handle; when the handle ends, the node ends.* The
runtime kills the owner, and the release path (§2.4) does the rest.

**The owner may be hibernating when it happens**, and that is the case a
parked service actually reaches after hours of quiet. So the kill has to
work against a snapshot rather than a running instance: there is no guest
to notice, nothing to unwind, and the release path (§2.4) runs against a
slot whose `inst` is already `None`. A vital handle that only fired on
resident owners would have its gap exactly where this design leans on it.

**A vital-handle kill is its own variant in the event a supervisor sees,
never a reason string on a generic kill.** A supervisor restarting on
abnormal exit has to tell "its socket closed, so the runtime ended it" —
a *normal* end — from a trap. Folded into one event, it either
restart-loops a service that ended correctly or misses a crash.

This is the socket-owns-node arrangement, reached from the other side, and
it costs one flag. A node may still hold ordinary handles it survives; a
service node declares its one socket vital and the guarantee is the
runtime's rather than the program's.

### 3.4 Readiness without residency

A node with a quiet socket hibernates. The connector pushes to the
owner's queue when its handle becomes readable, and the push wakes it —
`wake_on_message` already being the wake path.

This is the block pattern, pointed at one node instead of a config-named
queue: `turn`, `relay` and `wireguard` already push to node queues on
their own intervals. The mechanism is proven; what is new is the address.

### 3.5 Two handle namespaces, and why they differ

A queue handle and a resource handle follow different rules, and the
difference is not arbitrary — they have **different issuers**.

- A **queue handle is issued by the instance**. It is "runtime identity —
  valid only for the instance that issued it", so it is cleared wherever
  `inst` changes: build, hibernate, wake (`crates/drt-swarm/src/swarm.rs`).
  Hibernation snapshots and drops the instance, so the handle names a
  queue in an instance that no longer exists. The parked wait set is
  dropped for the same reason.
- A **resource handle is issued by the host**, keyed by `InstanceId`,
  which is the same id across `hibernate` and `wake`. Rebuilding the guest
  does not invalidate it.

**Traced, because it decides the shape of every guest program.** A guest's
handles *do* survive. A whole-instance snapshot carries the queue
subsystem's state — `diluvium_snap` writes it under `queues`
(`src/dsnap.c`) — and that state is a plain table of numbers, strings and
tables, so restoring it verbatim brings the queues, their contents and the
program's own handles back unchanged. diluvium's own implementation note
says it outright: *"Handles do not go stale after all."*

So for guest code the shape is **wake and continue**. A service node is a
loop, not a state machine with a re-entry step, and nothing needs a
re-`lookup` prologue.

The clear on DRT's side is not in tension with that. `slot.handles` is the
*host's* name-to-handle memo, cleared because the host cannot assume the
`inst` pointer it interned against is the same object; the guest's handles
live in the snapshot and DRT's memo does not. Two caches, one of them
guest-visible and durable, the other host-side and rebuilt.

**One correction is owed upstream, and it is not this repository's to
make.** `doc/Messaging.md` §10.8 still reads "Queue handles do not
survive… Any handle value stored in program state is stale and must not be
silently reused", which is what the design intended before the whole-
instance snapshot made it unnecessary. §10.8's re-declare-by-name step is
still needed for the case it was actually written for — moving one
program's state into an instance that already has queues, which
`diluvium_queue_setstate` refuses rather than merging two numbering
spaces. Anyone reading §10.8 alone will build the prologue this section
says is not needed.

### 3.6 Which surface goes down which path

This release is **additive**: a new connector, not a change to
`crates/drt/src/listen.rs`. Both paths exist afterwards, and the rule for
choosing is about who parses the bytes.

- **The root-owned listener, for protocols the host parses.** It is a
  queue bridge and it gives HTTP request parsing, the header allowlist,
  and the one-request-per-connection discipline. Everything REST-shaped
  stays here. A guest reimplementing HTTP on a raw socket would be
  throwing away a working parser and a deliberate allowlist.
- **A node-owned socket, for protocols the node speaks.** It gives bytes.
  A long-lived WSS leg, a datagram port, a framed protocol of the node's
  own — anything whose lifetime is the service's rather than the request's.

For the consumer design that means the mutating verbs keep arriving on
the root's queue while its `park` and `advertise` become node-owned — the
split it already implies without saying so. Its confirmation, on review:
*"§3.6's split is the one we'd have drawn."*

### 3.7 What this buys, and what it does not

**It bounds a faulty client.** Malformed input, a slow-loris, abusive
traffic: the handler node absorbs it, its budget caps it, a trap kills it,
its siblings continue. That is the common case and this is the right tool
for it.

**It is not process isolation.** Everything is one OS process. A bug in
the socket connector itself is still system-wide, and a tenant cannot be
contained from a host-side fault by this mechanism. Node isolation is
guest-fault isolation. Anyone who reads this document as "a tenant can no
longer take down the system" has read it wrong.

### 3.8 Why node-per-connection is affordable here

A hibernated node costs roughly **1.4 KB** cached in the benchmark's
churn case (256 agents, an eighth resident:
`bench/drt-bench-run.json`). Idle connections cost nearly nothing and the
working ones are the resident ones. That is what makes "spawn a node per
client" reasonable where thread-per-connection would not be, and it is the
strongest argument for building this at all.

## 4. `crypto/derive { label }` — a runtime name for a host-held subkey

### 4.1 The problem it solves

`crypto.secrets` is a map of host-held named secrets and `crypto/hmac`'s
`key` argument names one (`connectors/crypto/src/lib.rs`). That is the
right shape and it is config-time only. discofetch creates rooms, tokens
and peers **at runtime**; a room made at 14:02 has no entry in a config
written at deploy time.

A guest can already build HKDF-expand out of the `hmac` that ships. The
reason not to is that the derived key then lives in the guest heap, **and
the guest heap is the snapshot** (§3.5: a whole-instance snapshot carries
guest state verbatim). That defeats the property the connector's own
header claims — the signing key lives in the host and never in a guest,
in neither its heap nor its snapshot.

### 4.2 The call

```
    crypto/derive { label = "room:" .. room_id }    -- returns nothing
    crypto/hmac   { data = msg, key = "room:" .. room_id }
```

**Returning nothing is the whole point.** The guest gets a name it can
pass to a signing call, and never the bytes.

Both halves exist. Named-secret lookup is `keys.secrets.iter().find(...)`
in the `hmac` path. Keyed derivation from a master is `Keys::derive`,
which already computes `hmac_sha256(&master, KDF_LABEL_HMAC)` and the same
for JWT. This is those two wired to a guest-callable name.

### 4.2a A label registers a *master*, and never signs directly

The connector's header already states the rule this must obey: *"The
configured secret never signs directly. Two independent subkeys."* The
reason is written three lines below it — a program holding only
`host:crypto/hmac` could MAC a JWT signing-input itself and assemble a
token, bypassing `host:crypto/jwt_sign` entirely. `do_hmac` signs with
`k_hmac`, `jwt_sign` with `k_jwt`, and the two are independent.

**A label follows the same rule, one level down.** `derive` registers a
label *master*; each consumer derives its own subkey from it under the
existing labels:

```
    hmac   uses  hmac_sha256(label_master, KDF_LABEL_HMAC)
    jwt    uses  hmac_sha256(label_master, KDF_LABEL_JWT)
```

One name, two non-interchangeable keys, the separation preserved
per-label. It is the same two lines as `Keys::derive`.

Without this the hole is exact and it is one call wide: a guest with
`host:crypto/hmac` and no `jwt_sign` builds `"<header-with-kid>.<claims>"`
itself, MACs it under the shared label, concatenates, and `jwt_verify`
accepts it. That is the oracle the subkey split closed, re-entered through
a name instead of a key.

**A consequence that has to be said out loud:** the JWT key reached by
`kid = "partner-x"` is *not* partner-x's shared secret. Nobody should read
a `kid` as naming the bytes their peer knows.

### 4.3 Derivation is deterministic, and that is forced

`derive` is a **pure function of (master, label)**. It has to be: §3.6
puts the mutating verbs on the root's queue and `park`/`advertise` on
node-owned sockets, so **the signer and the verifier are different
owners**, and cross-owner verification only works if both reach the same
bytes from the same label. `Keys::derive` is already deterministic; this
is the same property one level down.

Two things follow, and the second is the one that matters.

**A second `derive` of a live label is idempotent, not a refusal.** A
refusal would leave the second owner unable to reach the key at all.

**The lifetime rule is bookkeeping; the scope on `derive` is the actual
control.** A deterministic label is re-mintable by anyone permitted to
name it, so `host:crypto/derive` without a scope on label strings means
any node holding it can derive `room:2` and sign for a room that is not
its own. A per-room key whose blast radius any sibling can cross is not
buying the isolation §5 offers it for.

### 4.3a A label has an owner, and `derive` has a scope

**Lifetime**, from §2.3, with no new rule needed: `node` by default,
released when the owner dies, `root` if declared, surviving hibernation
exactly as a socket does (§2.4).

**Reach**, which is the security-relevant half: `host:crypto/derive`
carries a **prefix scope**, the `allow` shape §3.1 already cites from
`connectors/exec`. The node owning `room:2` may derive `room:2`; a room
manager gets `room:*`; nothing else may name either.

Node lifetime was chosen over the process scope that was asked for, and it
is the better default — but not on security grounds, and the earlier draft
of this section was wrong to say so. Re-derivation on boot still works
because a label is derived rather than stored.

### 4.4 The refusals

- A label that collides with a configured `crypto.secrets` name is refused
  at the `derive`, naming both. Silently shadowing a config-time secret
  with a runtime one is the kind of thing that is discovered by a signature
  that verifies on one host and not another.
- `hmac` with an unknown key name already refuses; an expired or released
  label reads as unknown, which is the same sentence.
- `derive` is its own capability, `host:crypto/derive`. Holding
  `host:crypto/hmac` does not imply the right to mint new keys.
- **A label outside the capability's prefix scope is refused** (§4.3a),
  and the refusal names the scope rather than whether the label exists.
  Whether `room:3` has been derived is not `room:2`'s owner's business.
- **`jwt_sign` naming a `crypto.secrets` entry is refused** (§5.2). Those
  bytes are shared with a peer by definition, and no derivation from them
  is safe against the party that holds them.

## 5. `kid` in the JWT header, and the algorithm from the named key

### 5.1 What is there now, and what it costs

`jwt_sign {claims, ttl?}` and `jwt_verify {token}` sign with one host key.
The header is a **constant** — `{"alg":"HS256","typ":"JWT"}`, base64url,
unpadded — and `jwt_verify` compares the header segment against that
constant rather than reading an `alg` out of the token. That closes alg
confusion structurally, and it stays.

The consequence is that there is exactly one signing key for the life of
the host: no rotation without invalidating every live token, and no
per-issuer or per-room key.

**The argument for doing this now is the format, not the crypto.** Adding
`kid` later means every token already issued is re-minted or accepted
through a second verifier during a migration window. Adding a second
algorithm later turns the byte-wise header compare into a set compare — a
security-sensitive edit made under whatever pressure created the need for
it. Both are cheap today.

### 5.2 The shape

```
    jwt_sign   { claims, key = <label> }    -- header carries kid = <label>
    jwt_verify { token }                    -- kid selects the key
```

Two properties held, and the first is the one that matters:

- **The verifier is selected from the named key, never from the token's
  `alg`.** Look up `kid`, take that key's type, rebuild the header that
  key would produce, compare byte-wise. It is today's property with one
  indirection in front of it: still no field a token can set to change how
  it is checked. An unknown `kid` refuses; a header that does not match
  the rebuilt one refuses.
- **`key` accepts a §4 label, and never a `crypto.secrets` name.** A label
  reaches its JWT subkey (§4.2a); a per-room or per-issuer signing key is
  therefore reachable at runtime, which is the point of the ask.

**Why a `crypto.secrets` name is refused, and why per-consumer derivation
is not enough on its own.** Those entries are *by definition* bytes a
third party holds — `do_hmac`'s own comment says so: "a peer computes with
the same bytes — a webhook signature." Deriving a JWT subkey from them
does not help, because `KDF_LABEL_JWT` is a public constant in
open-source code, so the peer can compute `hmac_sha256(their_bytes,
KDF_LABEL_JWT)` themselves and mint tokens this host would accept. The
subkey split protects a key from a *guest* that never sees the bytes; it
cannot protect one from a party that already has them. So `jwt_sign` with
a `crypto.secrets` name is refused by name, saying that those bytes are
shared with a peer and cannot back a token this host trusts.

**Two properties the rebuild-and-compare gives for free, worth stating
rather than leaving as consequences.** Because verification rebuilds the
header the named key would produce and compares bytes, a token carrying
two `kid` keys and a token whose header members are in a different order
both fail — without any parsing rule about duplicates or ordering, which
is where that class of bug usually lives.

### 5.3 Tokens already issued keep working, behind a switch

A token with no `kid` rebuilds to exactly today's constant header and
verifies against the default derived key, so nothing already issued
breaks.

**That fallback ships as config, not as a promise to remove it later.**
`jwt.accept_unkeyed`, **default `true`**. The earlier draft called it
"removable later", which walked straight into the thing §5.1 exists to
avoid: removing it *is* a migration window, for the exact case `kid` was
wanted for — retiring the default key. One boolean now makes that a config
change rather than a release.

Default `true` costs nothing and is correct for anyone holding tokens
already issued. **Its first consumer sets it `false` from day one** —
discofetch has no live DRT-signed JWTs, since their API tokens are
prefix-and-hash rather than JWT, so there is nothing to migrate and no
reason to carry the branch. That also answers the worry that a default-on
compatibility switch never gets turned off: acceptance 21's "refuses with
it off" is a path exercised in production from the first deploy rather
than a theoretical one.

It is also the one place §5.2's "never branch on what the token said" is
not quite true: `kid` present versus absent is a branch on the token, and
it selects a key. The switch is what bounds it — with `accept_unkeyed`
off, the branch is gone and every token names its key.

### 5.4 Asymmetric is not in this release

EdDSA/ed25519 is wanted when a self-hosted fetchpoint's tokens must verify
without the issuer online, and that is open on discofetch's side rather
than shipped. The `kid`-plus-algorithm-from-key-type shape above makes
adding a key type **additive** rather than a format break, which is the
part that has to be right now. If ed25519 lands cheaply alongside, take
it; if it does not, nothing is lost.

## 6. The park deadline outlives residency

### 6.1 The one-liner half, and not the other one

`detached` drops the park (`crates/drt/src/start.rs:186`), so a hibernated
instance has no timer. `doc/Next.md` already priced this: most of the
timer exists — `next_deadline` computes the earliest across the roster and
the drive loop sleeps toward it — and the residency half is nearly a
one-liner, while the snapshot-store half is weeks.

**Only the residency half is in scope.** The deadline outlives `detached`
and `next_deadline` considers hibernated instances. A deadline still dies
with the process, and that is deliberate: scheduled things surviving a
restart means the deadline belongs in the snapshot store, which is a
different release.

### 6.2 What it is not for

Not row expiry. discofetch handles that lazily and it works: a read-time
predicate for correctness, a counted sweep on the write path, a boot sweep
for restart. All three ship already and need nothing from DRT.

### 6.3 What it is for

**A parked node whose peer died quietly.** Readiness (§3.4) fires on bytes
arriving. Nothing fires on bytes *not* arriving, and the one case lazy
evaluation structurally cannot reach is a node waiting on something that
will never come.

It is §11's risk 5 with the arrow reversed. There, a deadline firing too
eagerly wakes a node that should have stayed parked. Here, no deadline at
all parks a node forever. Both are the same mechanism read from opposite
ends, which is why they belong in one release.

### 6.4 A deadline is a guest-chosen wake rate

With the deadline surviving hibernation, **a guest's own choice of
deadline becomes a wake rate for a parked node**, and `enforce_residency`
re-hibernates it straight after. A short recurring deadline is a
rebuild loop, and §3.8's affordability argument assumes parked nodes stay
parked. It is a footgun one guest can point at the whole root.

It is still the right feature and it is asked for. It ships with a
**configured floor of 30 seconds** on how short a deadline a parked
instance may arm.

The number comes from the right quantity, which is not the per-node
rebuild cost: it is **aggregate wakes per second across the parked
population**. Five hundred parked nodes at a 30-second floor is about 17
wakes a second, which is nothing. The same population at a one-second
floor is 500 a second, which is a different machine. Thirty seconds is
three orders of magnitude above a rebuild and far under the
minutes-to-hours deadlines this exists for, so it costs their author
nothing.

**The floor is a proxy for the quantity that actually matters**, and that
is worth writing down before somebody raises the parked count tenfold and
rediscovers it. The correct long-term control is a cap on aggregate wakes
per second, because the floor is per-node and the cost is
population-wide. Not worth building now; §11 risk 5 carries the note.

### 6.5 What fires

The deadline **wakes** its owner; it does not kill it. A guest sees the
timeout on its own wait and decides what that means — unlike a vital
handle (§3.3), where the runtime decides because the resource is gone. A
timeout says "nothing arrived", which is information, not a verdict.

## 7. Plugins: the design is settled and the build is deferred

### 7.0 Why it waits

`PluginConnector` is **not in this release**, and the reason is risk
budget rather than dependency. If §2 lands first, plugins are technically
unblocked: caller identity and per-instance release are exactly what
per-node plugins need, and both scopes would work with no manifest churn.

What does not fit is §7.3 — routing on `(family, scope key)` instead of a
name touches the path every hostcall takes. §3 already carries caller
identity, per-instance release, the handle table, vital handles, two verb
sets, accept-and-transfer and the readiness push. Adding a change to the
universal hostcall path on top of an expedited release is how a schedule
becomes a rollback.

There is a design reason as well as a scheduling one. **Sockets are the
better first consumer of §2** — they exercise it harder than plugins do:
transfer, vital handles, hibernation, readiness. Plugins as the second
consumer then *validate* the abstraction rather than co-designing it, and
if it is wrong that is one consumer's rework instead of two.

So §2 must be built **resource-agnostic, not socket-shaped**: `release`
walks resources rather than sockets, caller identity lands on the
connector trait rather than on a socket-specific path, and the handle
table is keyed by owner and kind rather than by fd. Free if decided now,
and the whole difference between plugins being a follow-on and plugins
being a second implementation. §4's labels are the cheap proof that it
generalised, since they are a host-held resource that is not a socket.

**What is already built and stays put:** `crates/drt-plugin` — the frame
codec, the session state machine that multiplexes calls over one stream,
and the unix transport that execs a plugin with its channel on fd 3.
Thirty-eight tests, nine of them against a real subprocess. Nothing wires
it into `drt`, so it adds nothing to the release artifact.

One cost of deferring, stated so it is not a surprise: a library nothing
consumes is unproven in a way tests do not fix. That channel has never
carried a real workload, and the first true integration should be expected
to find something — most likely in readiness timing, or in how a plugin's
death surfaces to a caller. Which argues for plugins being the *early*
part of the next release rather than the late part.

### 7.1 The scope question, settled the other way

`doc/Plugins.md` assumed one plugin process serving many callers — the
manifest's `max_inflight` is written for it. That assumption came from the
C host, which had no nodes to own anything.

In DRT an implicitly shared plugin is an **ambient singleton**, which is
the thing the node and capability model exists to avoid. So the default
inverts: a plugin declares its scope (§2.3), and absent a declaration it
is `node` — an instance belonging to the node that called it.

`root` stays available, because some things genuinely are singletons and
forcing their authors to write a fronting node would be make-work. What it
costs is stated rather than hidden: a `root`-scoped plugin serves several
nodes and **cannot tell them apart**, since caller identity stops at the
host. Any per-caller policy is then the host's to enforce before the call,
never the plugin's. A `node`-scoped plugin gets that isolation
structurally and needs no such care.

Three arguments for sharing were considered and all three fail on DRT's
own terms:

- *It binds a port or owns a device.* Then the second instance fails to
  bind, exactly as a second process would. The OS already arbitrates, and
  the failure is named rather than prevented.
- *Its value is shared state — a pool, a cache, a limiter.* Then it is a
  resource with a concurrency model, and designing for that is the node
  author's work, not something the host should decide by making the
  process ambient.
- *It is expensive to start.* Also the node author's call.

### 7.2 Shared *state* is still composed

`root` scope shares a **process**. It does not make that process a safe
place to keep state several nodes read and write: a pool, a cache, a
limiter still wants an owner and a concurrency model, and in this system
that owner is a node that holds it and fronts it on a queue.

So the two are not alternatives. Use `root` when the resource is a
singleton by nature — it binds one port, it owns one device. Use a
fronting node when what is shared is state.

### 7.3 What changes, when it is built

The dispatcher routes **family → connector** today. It becomes
**(family, scope key) → instance**: lazily started on first call, released
when its key's owner dies (§2.3, §2.4). This is the riskiest change in the
release and the one that depends hardest on §2.1.

### 7.4 What the channel work already gives it

Built, green, and unchanged by this: the frame codec, the session state
machine that multiplexes calls over one stream, and the unix transport
that execs a plugin with its channel on fd 3. `max_inflight` stays
meaningful — one node may have several calls outstanding.

A plugin still cannot answer `denied`. That word is the dispatcher's, and
per-node instances do not change who may say it.

## 8. Inter-root: the prerequisites, not the feature

**What lands:** the `Channel` generalisation. A stream obtained by dialing
rather than by forking is `doc/Plugins.md` §4.1's `tcp` row, and it is
`ProcessChannel` minus the fork. A peer link needs that and so does a
plugin over a socket.

**What does not land:** delivery. Its blocker is a document rather than
code — wire protocol, framing, replay protection, and per-message signing
versus a session handshake (`doc/Peers.md` §5). Nothing in this release
moves any of the refusals, and `peer delivery is not supported in this
build` still says exactly what it says today.

**What must not be done:** one frame vocabulary for both. A plugin frame
that grew an optional signature field would blur which side is trusted.
Two protocols over one `Channel`; never one protocol.

The reason to name inter-root here at all is sequencing: the transport a
peer link will need is the transport a plugin uses, proven against a real
subprocess first, in a release that was not blocked on it.

## 9. Acceptance

1. A connector can learn its caller, and the nine existing connectors
   compile without edits.
2. A handle used by a node that does not own it reports **no such
   handle**, and the message does not reveal that it exists elsewhere.
3. A node that is killed has its handles closed; a test observes the fd
   count return to its starting value.
4. A node that **hibernates** keeps its handles, and a write from the far
   end wakes it — the readable half of the parked-service case.
5. A release that loses work reports it, attributed to the node.
6. A node accepts a connection, spawns a child, transfers the handle, and
   the child serves the connection to completion.
7. A child that traps mid-connection is killed; its socket closes; its
   siblings and its parent are unaffected.
8. A plugin that declares `node` scope, or declares none, belongs to its
   calling node: two nodes calling one family get two processes, and each
   dies with its owner.
9. A plugin that declares `root` scope gets one process for the root: two
   nodes calling it reach the same one, and it outlives either of them.
10. A plugin declaring a scope that is neither is refused at load, by name
   and with both spellings in the message.
11. A handle declared **vital** kills its owner when it closes — and the
   test runs it **against a hibernating owner**, because that is the state
   a parked service is in when the far end finally hangs up. The node is
   gone without running a line of its own logic and its other handles are
   released, with no guest resident to notice. This is acceptance 4's
   socket in its other state, and the one that would leave a gap exactly
   where a parked-service design leans on the guarantee.
12. A datagram handle binds, receives from several senders and sends to
   each, and is released with its owner like any other.
13. A node that hibernates and wakes **keeps both its queue handles and
   its socket**, and reuses each without re-resolving anything. This pins
   §3.5 against the upstream behaviour rather than against §10.8's
   superseded wording, so a change on either side is caught here. **The
   test names the diluvium revision it passed against**, because the fact
   it rests on is an implementation note rather than a documented
   contract, and this repository already pins diluvium by revision.
14. An acceptor spawns a child per connection and the child joins its
   subject by message; no second listener is bound per subject.
15. `crypto/derive` registers a label, `crypto/hmac` signs with it, and
   **the key is nowhere in the guest** — the test snapshots the node after
   signing and searches the whole stream for the derived bytes.
16. **The negative that makes 15 mean anything:** a token assembled by
   hand and MAC'd through `crypto/hmac` under the same label **does not
   verify**. The positive test passes with or without the subkey split
   (§4.2a); only this one fails without it.
17. `jwt_sign` naming a `crypto.secrets` entry is **refused**, saying
   those bytes are shared with a peer.
18. Two different owners deriving the same label reach the same key, so a
   token signed by one verifies in the other — the cross-owner case §3.6
   forces — and a second `derive` of a live label is idempotent.
19. A node whose `derive` scope allows `room:2` **cannot** derive
   `room:3`, and the refusal names the scope rather than the label's
   existence.
20. A derived label dies with its owner, survives its owner's hibernation,
   and a label colliding with a `crypto.secrets` name is refused at the
   `derive` naming both.
21. A token signed under `kid = <label>` verifies; the same token with its
   header swapped for another key's refuses; an unknown `kid` refuses; a
   token with two `kid` keys refuses; and a token with **no** `kid`
   verifies with `jwt.accept_unkeyed` on and **refuses with it off**.
22. A hibernated instance's park deadline still fires, waking it rather
   than killing it, and the guest sees a timeout on its own wait. A
   deadline under the configured floor is refused when it is armed.
23. A vital-handle kill reaches a supervisor as **its own event variant**,
   distinguishable from a trap without parsing a reason string.
24. Every refusal in `doc/Peers.md` still refuses, unchanged.

## 10. Not here, named so nobody builds it early

- **A plugin process per connection.** A node per connection is cheap; a
  process per connection is not.
- **Inter-root delivery.** §8.
- **Handle transfer across roots.** Transfer is within one swarm. A handle
  that crossed a root boundary would be a capability escaping its ceiling.
- **Per-tenant process isolation.** §3.7. A root is a process; this
  release does not change that.
- **WebSocket framing and TLS termination.** The stream verbs are listen,
  accept, read, write, close. A WSS service terminates TLS outside and
  proxies in; the websocket framing above the bytes is a connector or a
  plugin, and neither is here.
- **Scheduled wake.** §3.4 gives wake-on-readable and nothing gives
  "wake me at T". Expiry under hibernation wants a timer connector pushing
  to a node's queue — the pattern §3.4 describes, aimed at a clock instead
  of a socket. The pattern exists; the connector does not.

## 11. Risks, in the order I would worry about them

1. **The dispatcher change (§7.3), when plugins are built.** Routing on a pair rather than a name
   touches the path every hostcall takes. It wants the tightest test.
2. **Handle transfer (§3.2)** is new surface with no precedent in this
   repository, and it is a deliberate hole in an invariant.
3. **`sql`'s path-keyed cache — answered, and now a constraint rather
   than a risk.** Two nodes *can* name one database. One
   `SqlConnector::new()` is wired for the root
   (`crates/drt/src/cli.rs:769`) with one scope from config; `resolve_db`
   joins a name under that one root and the cache is
   `Mutex<HashMap<PathBuf, Connection>>`, never evicted. Two nodes naming
   the same database get the same `Connection`, so a `BEGIN` in one and
   writes in another interleave on one transaction.

   §2.3 says what it is — `sql` is `root`-scoped and the fix is to state
   that rather than rework it — but stating it in this plan is not enough.
   **The constraint goes in the sql connector's own documentation in this
   release: one database file per node, or single-statement writes only.**
   The failure reports through `finish` at teardown, which is where anyone
   would first hear of it, and that is far too late.

4. **A parked node that never wakes.** Readiness push (§3.4) is the only
   thing standing between a hibernating service and a connection that
   silently goes unserved. It wants a test with a real socket, not a
   loopback.

5. **A guest-chosen deadline is a wake rate (§6.4).** A short recurring
   deadline on a parked node is a rebuild loop, and §3.8's affordability
   argument assumes parked nodes stay parked. The 30-second floor is the
   mitigation.

   The risk is not that the floor is wrong; it is that **the floor is a
   proxy for the wrong quantity**. It is per-node, and the cost is
   population-wide: the same floor that is generous at 500 parked nodes is
   not at 5,000. The correct control is a cap on aggregate wakes per
   second. Not worth building until the parked count justifies it, and
   worth having written down here so that whoever raises that count by an
   order of magnitude finds this paragraph instead of the symptom.

## 12. The order of work

**Before any of it, and not in the list because it is not ours to do:**
file the `doc/Messaging.md` §10.8 correction upstream. Until it lands, two
public documents disagree about whether queue handles survive, and anyone
building a service node from §10.8 writes the re-`lookup` prologue §3.5
says is unnecessary. It blocks nothing here and should not wait on
anything here either.

Each step below ends green and nothing depends on a step after it.

1. **The ownership dimension** (§2), resource-agnostic from the first
   commit: caller identity on the trait, a handle table keyed by owner and
   kind, `release(owner)` on the death path, and a release that can say
   what it lost. No sockets yet.
2. **`crypto/derive`** (§4). Two connector halves wired to a name, and the
   proof that §2 generalises past sockets while it is still cheap to
   change if it does not.
3. **Stream sockets** (§3.1–3.2): listen, accept, read, write, close, and
   the transfer verb.
4. **Vital handles** (§3.3), including the hibernating owner.
5. **Readiness push** (§3.4), and the acceptance-13 test that a woken node
   keeps both its queue handles and its socket.
6. **Datagram sockets** (§3.1), the second verb set on a mechanism already
   proven by the first.
7. **`kid`** (§5). Independent of everything above; can move earlier if
   someone is free, since it touches only the crypto connector.
8. **The park deadline** (§6). The residency half only.
9. **The `Channel` generalisation** (§8), which is `ProcessChannel` minus
   the fork and lands nothing user-visible.

Steps 1 and 2 are the ones to get right; 3 through 6 are where the
schedule actually goes; 7 and 8 are small and separable, which makes them
the right things to hand to a second pair of hands.

**If anything slips, slip §3.** Steps 2, 7 and 8 — `derive`, `kid` and the
park deadline — unblock consumer work the day they land. §3 lands into a
Rooms design with unbuilt pieces on the consumer side regardless, so a week
there costs least. That is the consumer's own judgement and it matches how
§12 was already ordered.

## 13. Open, after two review rounds

§4 and §5 are settled — no further argument from the consumer on either,
and §5.2's refusal of a `crypto.secrets` name improved on what was asked
for. What remains is two items, and one of them is a number nobody can set
alone.

1. **Handle transfer (§3.2)** — still the only genuinely new API surface,
   still the thing most likely to be wrong, and now through two review
   rounds without anyone arguing with it. That is not the same as
   agreement, and it is the one place a reader should spend a second pass.
2. **Whether the guest datagram path has a launch consumer (§3.1).** The
   contradiction is resolved — the per-room relay port is a next-release
   path needing the node-scoped block, not a 0.7.0 one — so what is open
   is narrower: is there control-plane datagram traffic at launch? If yes,
   UDP earns its place in this release and its pps ceiling gets measured
   against that rate. If no, **UDP slips to the next release alongside the
   relay block**, and nothing else here changes. The consumer's own
   answer, on review: the comparison number can be given "once finding 1
   settles what that case actually is", which this section now does.

Closed since the first round, listed so nobody re-opens them by accident:
the wake-rate floor (§6.4 — 30 seconds, with the note that it is a proxy
for aggregate wakes per second); `jwt.accept_unkeyed` (§5.3 — default
`true`, and its first consumer sets it `false`); the `sql` question (§11,
risk 3 — yes, definitively, and now a constraint in that connector's
docs); whether verification branches on the token (§5.3 — yes, bounded by
the switch); whether a vital-handle kill is distinguishable (§3.3 — its
own event variant); label lifetime (§4.3a — node scope, with the prefix
scope as the real control); and whether anything in §10 was assumed to be
in (nothing was).
