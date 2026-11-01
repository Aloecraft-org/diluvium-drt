# Wake policy: a threshold where there is a boolean

**Status:** proposed, not built. Nothing in this document exists in code.
It names one gap, the shape that would close it, and the surface that
shape touches. Where a claim rests on code it names the file.

**Scope: node-to-node delivery only.** This is not about hostcalls. A
guest waiting on the answer to a call it made is owed that answer and
there is no routing choice to make; `crates/drt-swarm/src/pump.rs` is
deliberately policy-free and should stay that way. What follows concerns
messages *between* nodes, where the host does choose.

---

## 1. The gap

Delivery to another node has four outcomes
(`Swarm::push`, `crates/drt-swarm/src/swarm.rs`):

| destination | outcome |
|---|---|
| resident | straight to the queue |
| dead or unknown | `Gone`, immediately |
| cached, `wake_on_message` set | bounded wake buffer, drained ahead of live pushes |
| cached, flag clear | `Gone` |

Two properties are load-bearing and are not in question here. A send
**never blocks**: a full queue is `Limit` straight back to the sender,
which decides inside its own deadline whether to retry. And a consumer
that is gone and one that is full look identical from the sender's side,
which is what keeps a request from hanging on a wedged consumer.

The gap is the third and fourth rows. `wake_on_message` is **one bit for
the whole node**: it is woken by everything, or it is `Gone` to
everything. The comment on `Swarm::wake_on_message` already concedes the
cost from one side -- hibernating a node without the flag "is not saving
memory, it is disconnecting a mailbox" -- but the other side is just as
real: a node that wants to be woken for anything important is woken for
telemetry too, and never gets to stay asleep.

There is no way to say *what kind* of message is worth a wake. That is
the gap.

## 2. What a node cannot currently say

- Wake me for a job assignment; let a heartbeat find me `Gone`.
- Wake me for a user-facing request; let a metrics sample drop.
- I am a batch worker: wake me for nothing, and I will drain on my own
  schedule.

Each is a sentence about the *receiver's* priorities, and each is
inexpressible today. Note that the sender's half of this is already
solved: the sender knows whether it is sending a heartbeat or a user
request, gets an immediate `Gone`/`Limit`, and decides for itself whether
to retry. The asymmetry is the point -- the receiver has no voice.

## 3. The shape

**A message carries a priority. A node declares the threshold it wakes
below.** Row three of the table becomes conditional:

| destination | outcome |
|---|---|
| cached, priority within threshold | wake buffer, as today |
| cached, priority outside it | `Gone`, as an unflagged node is today |

Three properties make this cheap rather than a rewrite:

1. **The boolean is the degenerate case.** `true` is a threshold that
   admits everything, `false` one that admits nothing. Existing behaviour
   is preserved by construction rather than by migration, and the four-row
   table keeps its four rows.
2. **The envelope already grows this way.** `Request` carries `args` as an
   optional field older readers skip (`crates/drt-hostcall/src/lib.rs`),
   and `Status` documents an explicit forward-compatibility discipline for
   exactly this: consumers switch on what they know. A priority field
   rides the same rule, so the wire format and guest portability hold.
3. **Nothing new can hang.** A message outside the threshold is `Gone`,
   which senders already handle on every send. The change adds a reason to
   return an existing answer; it does not add an answer.

## 4. What it touches

Honest surface area, because this is messaging core:

- **Per-instance state.** The flag lives in the instance slot, so a
  threshold does too, which means snapshot and restore carry it.
- **The deployment schema.** Where the flag is set today, a threshold is
  set instead, under the strict loader's rules -- so an unknown or
  malformed value is refused at load, by name, like every other key.
- **The C host.** `doc/HostBaseline.md`'s shared understanding of
  delivery. Two hosts that disagree about which messages wake a node
  disagree about whether a mailbox exists.
- **The wake buffer's bound.** It is already bounded (`LIMIT`,
  `crates/drt-swarm/src/lib.rs`); a threshold changes what competes for
  that space, not how much there is.

## 5. One caution about spelling

The obvious encoding is a byte, CAN-style, where lower means more urgent.
It is a real convention and it fits the domain. It also reads backwards:
`wake_below: 0x0a` does not look like "only urgent things" to anyone
meeting it for the first time, and a policy an operator cannot read off
the page is a policy they will get wrong.

Named levels that map to bytes keep the ordering and lose the trap. The
numeric form can stay underneath for a transport that wants it. Whatever
is chosen, the deployment file should be readable without a table.

## 6. Not this

- **Priority on hostcall replies.** No routing choice exists there; see
  the scope note above.
- **Priority as scheduling.** This is about whether a sleeping node is
  woken, not about the order resident nodes run in. Sharing a word for
  both would be a mistake.
- **Sender-side retry policy.** Already solved, and better placed: the
  sender knows its own deadline.
