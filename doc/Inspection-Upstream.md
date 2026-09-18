# Inspection, upstream: nothing reads another instance, and one hook slot

**Audience: whoever picks this up in `Aloecraft-org/diluvium`.** This is a
DRT document about the dv ABI, written here because DRT is where the need
surfaced. Nothing in it depends on DRT.

**Status: filed and being looked at**, as
[Aloecraft-org/diluvium#35](https://github.com/Aloecraft-org/diluvium/issues/35).
This document is the brief behind it and stays here as the DRT-side
record: what was measured, at which revision, and what DRT does in the
meantime. The issue is where the answer goes.

It was written before that issue existed because the repository had
issues disabled at the time — a fork of Lua inherits the setting — which
is why it takes the `*-Upstream.md` shape the other two in this directory
do rather than being a link.

Not urgent. DRT is proceeding with what works today (below) and would
rather not build around either gap if upstream intends to close them. The
two questions at the end are the whole ask.

Facts are read off `src/dv.h` and `src/dlibs.c` at the revision this tree
pins, `7f952d8`. Where this document reasons rather than quotes, it says
so.

---

## What already works, and is not asked for here

The sealed default keeps four of `debug`'s sixteen functions — `getinfo`,
`getlocal`, `gethook`, `traceback` — on the stated grounds that "a program
may read its own frames". That is load-bearing and it is enough for one
real shape of inspection: **an instance reads its own locals and reports
them over a queue**, with no flag and no ABI change. A static analyser
supplies names and slots; `getlocal` supplies the live values.

DRT's browser Instances panel can use that today for a cooperating
program. This document is about the cases it does not reach.

## Gap 1: nothing reads *another* instance's state

Of the thirty-four `dv_*` entry points, none reads a variable — not a
local, not a global, not a stack frame — and there is no table access of
any kind. `dv_usage`, `dv_memory` and the `dv_queue_*` family answer about
an instance's accounting and its mailboxes, never about its values.

So inspection exists only where the target is running *and* cooperating. A
tool cannot show:

- a hibernated instance's state, since it is not executing and cannot
  answer a query;
- an instance that is wedged or looping;
- an instance whose program was not written to report anything.

For a supervisor this is sharper. A parent can already `spawn`, `kill`,
`query` and `hibernate` a descendant — the supervision model exists and is
ancestry-gated — but "show me what this child is holding" has nothing to
call, even from a legitimate parent.

**One observation that may lower the cost.** The value-marshalling problem
is already solved: queues move msgpack across this boundary, so a read
returning msgpack would reuse machinery that exists rather than needing a
new value representation.

## Gap 2: stepping contends with the budget for one hook slot

`dv.h` states this directly, in the `DV_FLAG_UNSAFE_DEBUG` comment:

> `sethook` takes the one hook slot that `dv_set_budget` enforces a budget
> through -- so a program could switch its own budget off in one line.

A line or count hook for a debugger wants the slot the budget is already
using, and the budget's claim on it is precisely what makes `sethook`
unsafe to expose. Three further constraints shape anything built here, all
quoted or paraphrased from `dv.h`:

- §9.4 is emphatic that the instruction budget is for **"abort, not
  schedule"**.
- `dv_set_budget` refuses a limit on a *running* instance, because a
  budget changed mid-flight would make "exceeded" meaningless.
- `dv_usage`'s instruction count is "exact to the hook's granularity",
  which is coarser than stepping wants.

DRT's reading, offered as reasoning and not as fact: the mechanism looks
right — a hook firing on instruction counts is what a stepper is — so this
is an arbitration question rather than a missing feature. Whether the hook
can be multiplexed, and what a budget means while execution is paused, is
the design; the calls are downstream of it.

## The two questions

1. Is a read-only inspection entry point something upstream would
   entertain? If so, is a frame read (`getlocal`-shaped, by instance and
   level) or a named table read the better primitive?
2. Is the single hook slot a fixed constraint, or an implementation detail
   that could be multiplexed? That answer decides whether stepping is a
   small addition or a design change, and DRT would plan differently for
   each.

## What DRT does meanwhile

Cooperative inspection, per the first section. `DV_FLAG_UNSAFE_DEBUG` is
now wired through DRT (`allow_unsafe_debug`), so a tool can use `setlocal`
on a program its author controls — with the trade `dv.h` names, that with
the flag set the capability layer is a way of structuring a program rather
than a boundary around one. That is acceptable for a debugger and is not a
default: `drt run` and `drt repl` stay sealed.

Neither of those closes Gap 1. A panel still cannot show a variable in a
program that did not volunteer it, and no flag changes that — the flag
governs *writing* a value once something can reach it.

`doc/Supervision.md` §3 is the DRT-side sketch these questions feed.
