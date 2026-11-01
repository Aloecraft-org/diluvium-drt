# Supervision: what a node may do to another node

**Status:** §1 and §2 describe what exists and is tested. §3 is a sketch —
nothing in it is built, and it is recorded so the shape is not rediscovered
from scratch. Where a claim rests on code it names the file.

---

## 1. The model that already exists

A node supervises by writing to `system/lifecycle`. Four ops
(`crates/drt-swarm/src/swarm.rs`):

| op | what it does |
|---|---|
| `spawn` | start a child, with caps attenuated from the parent's |
| `kill` | end a descendant and its subtree |
| `query` | a descendant's state, plus `insns=` and `mem_kb=` |
| `hibernate` | park the requester, or a descendant |

**Authorisation is ancestry, not a grant.** `is_ancestor(parent, target)`
decides, and anything else is refused with "not a descendant". That is
worth stating plainly because it is the thing a capability-shaped design
would get wrong: there is no `supervise:*` to hold, and holding
`lifecycle` lets a node manage *its own subtree* and nothing else. A
sibling is not reachable at any grant level.

Spawning is rate-limited rather than filtered: a spawn over the per-step
limit is held and retried next step, in order, with one `throttled` event
per step telling the requester to back off.

## 2. The asymmetry worth knowing

`query` already answers a descendant's usage, so **a guest could see what
its children cost before a host could**. The host-side accessor
(`Swarm::usage`, and `usage(id)` in the browser exports) closed that, but
the direction of the gap is the interesting part: the guest-facing
supervision API was ahead of the host-facing one, because it was designed
for a supervisor program and the host was assumed to be a driver rather
than an observer. A panel is an observer.

## 3. The sketch: budget and inspection as supervisor ops

Two things a supervisor cannot do that it plausibly should.

### 3.1 Budget

Setting a descendant's budget fits the existing shape exactly: a new op in
the same match, gated by the same ancestry check, attenuated the way caps
already are — a parent may not hand a child more than it holds.

**One constraint is load-bearing.** `dv_set_budget` refuses a limit on a
*running* instance, because a budget that changed mid-flight would make
"exceeded" meaningless. So a supervisor sets a budget before its child
runs, or while the child is parked. That is not a workaround; it is what
keeps the word "exceeded" a fact. A supervisor wanting to throttle a
running child hibernates it first, which it can already do.

### 3.2 Inspection

Reading and writing a descendant's variables does **not** fit today, and
the reason is precise.

The sealed default keeps four of `debug`'s sixteen functions — `getinfo`,
`getlocal`, `gethook`, `traceback` — because "a program may read its own
frames". **Its own.** There is no entry point in the `dv` ABI's thirty-four
for reading another instance's frames, and none for reading a table by
name. So:

- **Cooperative inspection works now.** A node reads its own locals and
  replies over a queue. It needs no ABI change and no flag, and it works
  only for a running node written to answer.
- **Supervisor inspection needs an engine-side read.** A parent asking for
  a parked or uncooperative child's state has nothing to call. This is the
  ABI addition, and it is upstream.
- **Overriding needs `DV_FLAG_UNSAFE_DEBUG`.** `setlocal` is on the writing
  side. DRT now wires that flag (`allow_unsafe_debug`), so the mechanism is
  reachable — but it restores `getregistry`, `getmetatable` and `sethook`
  along with it, and `dv.h` is blunt about the consequence: the capability
  layer becomes a way of structuring a program rather than a boundary
  around one. Acceptable for a debugger on a program you wrote; not a
  default, and not something a supervisor op should imply.

### 3.3 Why stepping is not on this list

Breakpoints and stepping look like a third supervisor op and are not. The
instruction budget is enforced **through the one debug hook slot**, which
is exactly why `sethook` is refused by default — a program could switch
its own budget off in one line. A stepper needs that slot too.

So the work is not new calls; it is deciding how a budget and a debugger
share one hook, and what a budget means while execution is paused. Two
further constraints shape it: `dv.h` says the budget is for "abort, not
schedule", and instruction counts are exact only to the hook's
granularity, which is coarser than stepping wants.

The machinery is right — a hook firing on instruction counts is what a
stepper is — but the question is arbitration, and it is upstream's.

## 4. What the browser tier can and cannot show

After `usage(id)` and a session handing out its deployment, a panel can
show the roster, residency, capabilities, budgets and live cost of every
agent in a running deployment, and can push, kill, hibernate and wake.

It cannot show a variable. That needs §3.2's first bullet (the program
cooperates) or its second (the ABI grows a read). Nothing in the browser
exports changes that, and no flag DRT sets changes it either: the flag
governs writing a value once something can reach it.
