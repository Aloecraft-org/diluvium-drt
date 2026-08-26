# The hostcall encoding

> **Status:** normative. This document moved here from
> `aloecraft-org/diluvium` (`doc/Hostcall.md`) per SPEC.md §2; diluvium now
> references this copy. `crates/drt-hostcall` implements it as serde types
> and must never drift from this text.

The wire shape for a program asking its host for something the sandbox does not
contain — the time, a file, a fetch, a JavaScript function. `doc/Determinism.md`
carries the design argument and the measurement; the two-sentence version is that
**a hostcall is not an ABI**. It is a message on a queue the host drains and an
answer on a queue the host pushes to, which is why it gets replay for free: both
halves are already in the message log.

This file exists for one deadline and everything downstream of it. Determinism.md:
*"reserve a correlation token in the request encoding before the first hostcall
ships"* — a guest may have several requests outstanding (that is the point of not
blocking), replies arrive in whatever order the host answers, and a format without
the token forces either one-outstanding-at-a-time or a version break to add it.
This document is that reservation, written before any host — the lab JS host
included — ships a handler. A prototype that bakes in a token-less shape has made
the mistake this file is here to prevent.

## The request

A msgpack map, pushed by the guest onto its request queue:

| field | type | |
|---|---|---|
| `tok` | integer ≥ 0 | **The correlation token. Required.** Chosen by the guest, echoed verbatim by the host, never interpreted by it. Must be unique among that guest's *outstanding* requests; reuse after the reply arrives is fine. An integer rather than a string because it is compared, not read. |
| `call` | string | What is being asked: `"time"`, `"fs/read"`, `"js/invoke"`. Namespaced with `/` like queue names — structural, so a capability grant can cover a family. Required. |
| `args` | any | The call's arguments, in whatever shape the call defines. Optional; absent means no arguments. |

Nothing else is reserved. A call that needs more invents fields inside `args`,
not beside it.

## The reply

A msgpack map, pushed by the host onto the guest's reply queue:

| field | type | |
|---|---|---|
| `tok` | integer | The request's token, echoed verbatim. Required. |
| `status` | string | `"ok"`, `"denied"`, `"error"`, `"malformed"`. **The set will grow**; a guest switches on the values it knows and treats an unknown status as an error, which is what keeps growth from being a version break. There is deliberately no `"pending"` — under the queue shape every hostcall is already asynchronous, and "the answer has not arrived" is an empty queue, not a status (Determinism.md records the correction that removed it). |
| `value` | any | Present when `status == "ok"`: the answer. |
| `detail` | string | Present otherwise: why, worded for the program to read. The same field name the lifecycle events use, on purpose. |

**Every drained request is answered.** `denied` is for a call the guest is not
granted or the host does not connect; `error` is for a connected call that
failed; `malformed` is for a request the host could not read — echoing whatever
`tok` was readable, and omitting it when none was, which leaves an uncorrelatable
reply as the sender's own diagnostic rather than silence. A host that drops
requests on the floor has made backpressure invisible, which is the failure mode
the bounded request queue exists to surface.

## Queues and capabilities, the conventions a host implements

These are host-protocol conventions (`doc/Host.md`) rather than parts of the
encoding, stated here so the two ship together:

- The guest declares its request queue with `on_full = "reject"` — a host that
  stops draining must become a refusal the program can see, not a park. The
  reply queue is sized for the requests the program keeps outstanding.
- The `host` guest library (build7) is the surface a program normally reaches
  this protocol through; it allocates its tokens from `2^30` upward, so a
  program that also pushes raw requests on the same pair keeps its own tokens
  below that and the spaces never meet. One reply queue still cannot serve
  two consumers *concurrently* — mix sequentially.
- Which calls a guest may make is a capability question, resolved with the
  grammar the tree already has: a grant like `host:time` or `host:fs/*` covers
  `call` names the way `queue:work/*` covers queue names, attenuating through
  spawns identically. A call outside the grant is answered `denied`, never
  silently dropped — the same posture as every other refusal in this tree.
- Connectors are **off by default, all of them**. A host wires each one
  explicitly per environment; the guest cannot tell a real connector from a
  mock, and that indistinguishability is a feature the lab workflow depends on
  (prototype against a JS handler, deploy against a C one, guest unchanged).

## Metering, still open

A hostcall is not one VM instruction, so §9.4's budget does not naturally charge
for it. Determinism.md's placement argument stands: if a hostcall is a message,
the natural charge is per message and per byte, applied by the queue layer
rather than by a second accounting path only hostcalls use. Not settled here;
what this encoding guarantees is only that settling it later is not a format
change, because a charge is host-side arithmetic over fields that already exist.
