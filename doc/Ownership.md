# Ownership: a node holds a resource, and loses it when it dies

**Status:** spec, for the release after `0.6.2`. Proposed `0.7.0`; the
version is set when the first change lands, per `doc/Plan-2026-09.md`.
Nothing here is built. Where a claim rests on code it names the file.

This is one idea with three consequences. The idea is that a host-held
resource can belong to a *node* rather than to the root. The
consequences are node-owned sockets, scoped plugins, and the half of
inter-root that is transport rather than protocol.

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
(§8, risk 3).

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

### 3.1 What a node can hold

Listen, accept, read, write, close, capability-gated with a scope naming
what may be bound — the shape `connectors/exec`'s `allow` already
establishes for "which of these may a guest name".

A guest never holds a descriptor; it cannot, it has no syscalls. It holds
a handle, and the host holds the fd. "Node-owned" is a statement about
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

### 3.3 Readiness without residency

A node with a quiet socket hibernates. The connector pushes to the
owner's queue when its handle becomes readable, and the push wakes it —
`wake_on_message` already being the wake path.

This is the block pattern, pointed at one node instead of a config-named
queue: `turn`, `relay` and `wireguard` already push to node queues on
their own intervals. The mechanism is proven; what is new is the address.

### 3.4 What this buys, and what it does not

**It bounds a faulty client.** Malformed input, a slow-loris, abusive
traffic: the handler node absorbs it, its budget caps it, a trap kills it,
its siblings continue. That is the common case and this is the right tool
for it.

**It is not process isolation.** Everything is one OS process. A bug in
the socket connector itself is still system-wide, and a tenant cannot be
contained from a host-side fault by this mechanism. Node isolation is
guest-fault isolation. Anyone who reads this document as "a tenant can no
longer take down the system" has read it wrong.

### 3.5 Why node-per-connection is affordable here

A hibernated node costs roughly **1.4 KB** cached in the benchmark's
churn case (256 agents, an eighth resident:
`bench/drt-bench-run.json`). Idle connections cost nearly nothing and the
working ones are the resident ones. That is what makes "spawn a node per
client" reasonable where thread-per-connection would not be, and it is the
strongest argument for building this at all.

## 4. Plugins, scoped

### 4.1 The scope question, settled the other way

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

### 4.2 Shared *state* is still composed

`root` scope shares a **process**. It does not make that process a safe
place to keep state several nodes read and write: a pool, a cache, a
limiter still wants an owner and a concurrency model, and in this system
that owner is a node that holds it and fronts it on a queue.

So the two are not alternatives. Use `root` when the resource is a
singleton by nature — it binds one port, it owns one device. Use a
fronting node when what is shared is state.

### 4.3 What changes

The dispatcher routes **family → connector** today. It becomes
**(family, scope key) → instance**: lazily started on first call, released
when its key's owner dies (§2.3, §2.4). This is the riskiest change in the
release and the one that depends hardest on §2.1.

### 4.4 What the channel work already gives it

Built, green, and unchanged by this: the frame codec, the session state
machine that multiplexes calls over one stream, and the unix transport
that execs a plugin with its channel on fd 3. `max_inflight` stays
meaningful — one node may have several calls outstanding.

A plugin still cannot answer `denied`. That word is the dispatcher's, and
per-node instances do not change who may say it.

## 5. Inter-root: the prerequisites, not the feature

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

## 6. Acceptance

1. A connector can learn its caller, and the nine existing connectors
   compile without edits.
2. A handle used by a node that does not own it reports **no such
   handle**, and the message does not reveal that it exists elsewhere.
3. A node that is killed has its handles closed; a test observes the fd
   count return to its starting value.
4. A node that **hibernates** keeps its handles, and a write from the far
   end wakes it.
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
11. Every refusal in `doc/Peers.md` still refuses, unchanged.

## 7. Not here, named so nobody builds it early

- **A plugin process per connection.** A node per connection is cheap; a
  process per connection is not.
- **Inter-root delivery.** §5.
- **Handle transfer across roots.** Transfer is within one swarm. A handle
  that crossed a root boundary would be a capability escaping its ceiling.
- **Per-tenant process isolation.** §3.4. A root is a process; this
  release does not change that.

## 8. Risks, in the order I would worry about them

1. **The dispatcher change (§4.3).** Routing on a pair rather than a name
   touches the path every hostcall takes. It wants the tightest test.
2. **Handle transfer (§3.2)** is new surface with no precedent in this
   repository, and it is a deliberate hole in an invariant.
3. **`sql`'s path-keyed cache.** Not this release's to change, and §2.3
   is why it does not have to be: `sql` is `root`-scoped, and the fix is
   to *say so* rather than to rework it. What stays open is narrower and
   worth answering before an incident answers it — whether two nodes can
   name one database today, and therefore share one transaction.
4. **A parked node that never wakes.** Readiness push (§3.3) is the only
   thing standing between a hibernating service and a connection that
   silently goes unserved. It wants a test with a real socket, not a
   loopback.
