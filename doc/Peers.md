# Peers: the groundwork, and what is reserved

**Status:** the groundwork landed; inter-root communication did not, and
neither did plugins. This document says what shape exists, what refuses by
name, and what is deliberately not here so nobody builds it early. Where a
claim rests on code it names the file.

Nothing in this build talks to a peer. Everything below is either a shape
that is now fixed, or a named failure standing where a feature will go.

---

## 1. The model

- **The runtime does the talking, not the node.** A node writes to a
  foreign queue the way it writes to a local one, with a foreign address.
  Its runtime signs with the root's key and delivers to the peer's
  endpoint. The receiving node reads a queue and finds the verified sender
  attached by its own runtime. **No node ever sees a signature.**
- **Peers are part of the declared ceiling.** `project.json` declares
  expected peers by role, and that declaration is hashed with `caps`, so
  declaring an inbound peer prompts. The operator's binding of that role in
  `consent.json` is the acceptance.
- **Outbound is a grant.** A node may write to peer X only if it holds a
  cap naming X, attenuated from its parent like any other. Both sides
  check; either side alone can refuse.
- **Same queue semantics.** A cross-peer write is a queue write with an
  identity attached.
- **A peer is either another drt root or a plugin.** A root is addressed by
  its path on the same box; a plugin by the endpoint its platform provides.
  Neither is ever resolved through `~/.dollup/`.

A plugin is in this document because it is the same mechanism: a peer that
is not a drt root. Same contract, same consent, same caps; only the
endpoint differs.

## 2. What exists now

### The declared ceiling is two halves

`project.json` gained `peers`, and the hashed ceiling is `{caps, peers}`:

```json
"peers": [
  { "role": "db", "queues": ["query", "result"], "contract": "discofetch-db/1" }
]
```

A role is a name this root uses; `queues` are the ones it expects to write
to or read from; `contract` is a version string both sides declare, and
compatibility means the strings match. That is a flag day —
`discofetch-db/1` to `/2` has no overlap window and every root naming it
changes in one step. It is not semver and must not be read as one.

Both keys are always in the hash preimage, empty or not. An
omitted-when-empty `peers` would give one ceiling two hashes depending on
which code path built the value, and two implementers hashing different
bytes for one ceiling is the failure the shape exists to prevent
(`crates/drt-config/src/project.rs`, `DeclaredCeiling`).

**The widen relation gained a peers part and no others**: an added role is
a widening, a removed one a narrowing. Roles and nothing finer — the rule
is one an operator can hold in their head. The edge that leaves is real and
is asserted rather than left to be discovered: editing an existing role's
`queues` moves the hash but does not prompt
(`editing_an_existing_roles_queues_is_not_a_widening_today`). Whoever wants
a prompt for it changes `widen_check_ceiling`, and that test is what will
fail when they do.

### Upgrading a `consent.json`

The stored ceiling is now `{ "caps": [...], "peers": [...] }` where it used
to be a bare array. Both spellings read; only the object is written. An
entry from before `peers` existed hashes differently under the new
preimage, and that costs an operator nothing: identical caps and no peers
is a no-op edit, so it lands on the silent narrowing path and the entry is
rewritten in the new shape. `consent-listed-pre-peers.json` is the bytes
this repository actually shipped, kept unregenerated, with a test that
reads them.

### A node path can name its root

`QualifiedNode` is `<root_id>/root/intake` — **addressing only**. Hashed
objects carry `NodePath`, the local form, and `root_id` once as its own
field. Putting the qualified form in a preimage would hash the root twice
and let two implementers hash different bytes. The two spellings cannot be
confused: a local path leads with `root`, and a uuid7 never does.

### An address can name a peer, and refuses

`drt_config::peer::QueueAddress` carries an optional peer component, so the
hostcall ABI does not change shape when peer delivery lands. Every write
this build makes leaves it `None`. One that sets it is refused:

```
peer delivery is not supported in this build: 'query' on root 0192f0c1-…
```

The component is **not** typed as a `root_id`. A plugin has none, and a
type that could only hold a uuid would force the plugin case into a second
code path later.

### Every delivered message says who it came from

`Swarm::push` takes a `Sender` and attaches it under the reserved `from`
key: `{peer, node}`, where a local sender names its own root. A node reads
one field and never asks "was this one of mine?" separately, which is what
makes a node written today work unchanged when the first foreign message
arrives.

Two properties worth knowing:

- **An existing `from` is replaced.** The runtime is the authority on who
  sent something; a guest that writes the key itself is not believed.
- **Only a map carries it.** Every message in this system is a msgpack map,
  so the rule costs nothing in practice. A non-map message is delivered
  untouched rather than wrapped — wrapping would change what every existing
  reader sees in order to add a field none of them asked for. A non-map
  message that ever needs a sender needs an envelope, and that is a format
  decision with its own document.
- **It costs one allocation, and that is measured.** `bench/check-fidelity.py`
  holds `queue.pN_allocs_per_roundtrip` under 5.0 against a baseline near
  3.06. Decoding each message to an `rmpv::Value` and re-encoding cost three
  allocations and broke the ceiling at 6.06, so the delivery path reads the
  map *header*, writes a new one with the count raised by one, and copies the
  body after the sender's bytes: 4.06, and nothing at all for a non-map
  message. One allocation is the floor for adding a field to an immutable
  byte message; anything lower needs a different envelope.

`PeerRef` has a third form, `Runtime`, for the no-root path. `drt run` and
a browser root have no `root_id`, and "every delivered message carries a
sender" should not have an exception; the honest answer there is not a
made-up id. It is unaddressable, and a no-root deployment has no peers by
construction, so nothing is lost.

### A peer cap family

`host:peer/<role>` is registered with its realm, `operator.peer`. No
hostcall grants it and no connector declares it, so its scope type is
declared at load (`crates/drt/src/config.rs`). A scope on a peer grant is
refused by name: which queues a cap may narrow to is unsettled, and a scope
accepted now could mean something else later.

### A reserved binding, and a reserved endpoint

`consent.json` gained `peers`, the operator's binding of each role to what
satisfies it here:

```json
{ "peer": "db",       "kind": "root",   "root_id": "<uuid7>", "public_key": "<base64>", "queues": ["query", "result"] }
{ "peer": "webauthn", "kind": "plugin", "public_key": "<base64>", "queues": ["register", "assert"] }
```

The format is settled so that whoever writes the first binding writes the
final shape. Nothing implements it: a non-empty list is a named failure at
start. Refused rather than ignored, because a binding that parsed and then
did nothing would look, from where the operator stands, exactly like one
that worked.

`state/peer.sock` is reserved and nothing creates it. A path is the one
part of an endpoint that other software hard-codes, so it is claimed now
rather than by the slice working to a deadline. `SPEC.md` §13a is amended
when the listener lands.

## 3. Queue semantics, pinned

Queue writes are non-blocking. A write to a full queue is an immediate
named failure; the caller decides, within its own deadline, whether to
retry or give up, and an HTTP handler maps give-up to a 503. A consumer
that is gone and one that is full are indistinguishable to a sender, which
is the right property: the sender's behaviour is the same either way. A
plugin that is slow or absent looks the same to a node as a root that is.

Capacity is per queue, declared where the queue is exported.

**This governs the delivery surface, not the engine.** `WaitSet::for_space`
is a guest parking on its own push into its own full queue — `dv.h` §8.3,
inherited from the C host — and drt already reports it as a named failure
when nothing can drain it (`crates/drt/src/run.rs`). Retiring it would fork
the port, which is a decision with its own round.

## 4. Rules for code written now

So that the later split is configuration rather than a rewrite:

- Queues are the only channel between nodes. No shared globals, no reading
  another node's `live/` directory, no filesystem paths in messages.
- Every message carries the sender field and is handled by **reading** it,
  not by assuming who the peer is.
- Addresses go through `QueueAddress`, peer component `None`.
- Each node declares what it exports and consumes in its profile. That list
  becomes `peers[]` at the split.
- Caps are per node from day one. The root's ceiling is the union; each
  node's profile attenuates to what it alone needs. At the split each
  node's caps become its root's ceiling, unchanged.
- Logs go to the node's own `log/` path or the profile redirect. Nothing
  writes to a sibling's log.

## 5. Sequencing

The slice that builds peer delivery depends on node paths, the signing
helper, and the consent types — all of which are now in. It gets its own
document before any code: **wire protocol, framing, replay protection**,
and per-message signing versus a session handshake. Peer-gone behaviour is
not deferred; §3 above is the answer.

Endpoint types for plugins land after root-to-root, because that slice is
this one plus endpoint types.

## 6. Not here, named so nobody builds it early

- The wire protocol for root-to-root, and the listener on
  `state/peer.sock`.
- Endpoint types for plugins: ES module registration in the browser, a
  process on stdio or a socket natively, a wasm component under wasmtime, a
  wasip2 sibling component.
- The plugin package kind in dollup, the loader manifest, and identity
  pre-fill of a consent binding.
- The hostcall that grants a peer cap, and `request_grant` for peer
  admission.
- Cross-box transport over wssd or rtcsock. Same contract, different
  endpoint.
- Multi-root artifacts in push and pull.
- **Peer discovery of any kind.** Peers are operator-bound, like signers.
- The discofetch-api decomposition itself.

## 7. Open, and owned elsewhere

- **`doc/consent.md` is not in this repository** and never has been, on any
  branch or tag, though `consent.rs`, `gsr.rs` and `realm.rs` all cite it by
  section. The four amendments this round makes — the ceiling becoming
  `{caps, peers}`, the stored ceiling's shape, `node` being the local form
  in a GSR identity, and the peer family mapping to a realm like any other
  — are applied **to the types here**. Whoever holds that document has to
  apply the same four, or the spec and the code disagree silently.
- **`doc/Plugins.md` predates this and reaches a different conclusion.**
  Written 2026-09-03 against v0.4.2, its verdict is to implement the plugin
  channel as a connector *backing* behind the existing `Connector` trait,
  subprocess plus msgpack, and it says in as many words not to design a
  second protocol. §1 here makes a plugin a peer instead: reached by a queue
  write, bound in consent. Those are different designs, not two descriptions
  of one. The groundwork does not depend on which wins, and nothing was
  changed in that document — but the two cannot both stand, and reconciling
  them belongs to whoever scopes the endpoint slice.
- **The dollup half is not in this repository.** The `contract` field on the
  package manifest, `dollup audit` reporting declared peers, `dollup roots`
  showing the intended graph, and plugin delivery are all dollup's. So is
  acceptance 4.
