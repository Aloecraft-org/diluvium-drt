# The host baseline

**What every DRT host must answer, and what it must never do instead.**

A guest must not be able to tell hosts apart. That is the whole of the
doctrine — README's "the two are concentric, not parallel: there is never a
second runtime with different behavior for the same program" — and this
document is the operational half of it. It names the smallest set of
hostcall families a host has to answer, and the four rules that make an
*absent* family safe.

It exists because DRT is about to have more than one host. The native CLI
is one. `drt-web` is another (`doc/Browser.md`). The C host (`dhost.c`) is a
third and is the compatibility reference. Without a stated baseline, "which
calls work here" becomes something a program discovers by failing, which is
precisely the guide-divergence problem — *"sorry, that's different in
Lab"* — the whole browser-first plan existed to avoid.

## What a host is responsible for, and what it is not

The guest's libraries divide cleanly, and only one half is a host's problem:

| From the language core, free | The host's job |
|---|---|
| `queue`, `msgpack`, `bytes`, `json`, `endpoint`, and the pure `time` calendar (`time.iso`, `time.parse`, `time.fields`, `time.of`) | Draining `host/calls`, answering on `host/replies`, and the connectors behind them |

The `host` library itself — `call`, `capabilities`, `children`, `crypto`,
`events`, `exec`, `fs`, `monotonic`, `spawn`, `sql`, `time`, `try`, which is
the whole of it, verified by asking a guest for `pairs(host)` — also comes
from the core — it is a shim over that queue pair, `src/dhostlib.h`. A host does
not implement `host.time()`; it answers the `time` **call** that
`host.time()` makes. Get that distinction right and the rest of this
document is short.

## The baseline: three families

A family is on this list when **"I can't" is not a legitimate answer on any
plausible host.**

| Call | Answer | Why it is mandatory |
|---|---|---|
| `time` | wall-clock ms since the Unix epoch | The one nondeterminism every program eventually wants, and no program can synthesize it. A browser has `Date.now()`; an embedded host has a clock or it cannot schedule anything. |
| `time/monotonic` | ms on the host process's own epoch | Intervals — rate buckets, throttles, deadlines. Deliberately the same unit as `time` and never comparable to a persisted wall timestamp. Deriving it from `time` is wrong the moment the wall clock steps. |
| `crypto/random` | *n* CSPRNG bytes, hex | Entropy is unsynthesizable and a guest that rolls its own is a security bug. `crypto.getRandomValues` in a browser, `getrandom` natively. |

That is the entire baseline, and its shortness is the point. Everything
else — `fs/*`, `sql/*`, `ssh/*`, `exec/*`, listener queues, `lifecycle` — is
legitimately absent on some host, and the rules below make absence safe
rather than mysterious.

Note what is *not* here: nothing about grants. The baseline says a host must
**answer** these families, never that a given instance may **reach** them. A
deployment that does not grant `host:time` gets `denied`, on every host,
identically — and that is the baseline working, not a gap in it.

## The four rules

These matter more than the table.

1. **Every drained request gets a reply.** A host that pops from `host/calls`
   and does not push to `host/replies` parks the guest forever, and the
   guest cannot tell that from a slow connector. DRT's dispatcher guarantees
   this (`drt-connector`); a new host must guarantee it itself.

2. **An absent family is answered `denied`, by name.** Not dropped, not a
   timeout, not a raised host-side exception. The reply carries the
   connector's own sentence — `no connector is wired for 'sql/query' in this
   process` — and `host.try` hands it to the program as a status. Denial is
   an ordinary answer.

3. **A stub refuses; it never fakes.** This is the security-critical one and
   the only rule with teeth. A host that cannot supply entropy answers
   `denied`; it does not return zeros, a counter, or a PRNG seeded from the
   clock. Same for `time`: a host with no clock refuses rather than
   returning 0. A faked answer is indistinguishable from a real one to the
   guest, which is exactly what makes it dangerous.

4. **Mandatory means "implement or refuse explicitly", not "must succeed".**
   Rules 2 and 3 are what a host on the wrong side of rule 1 falls back to.
   A host may be missing a baseline family; it may not be silent or
   inventive about it.

## What this buys

**`drt-web` becomes tractable and, more importantly, becomes *checkable*.**
A browser host answers three families out of `Date.now()`,
`performance.now()` and `crypto.getRandomValues`, denies everything else by
name, and is *correct* — not a subset, not a degraded mode. A conformance
test can be written against this table and run against every host.

**The C host stays the reference.** `dhost.c` already answers all three
(`conn_time` is what `connectors/time` mirrors, refusal wording included).
Where DRT and the C host differ, the C host is right — that is priority #1
in `doc/Handoff.md`, and it is why `access` is `"read"` and not
`"readonly"`.

## Conformance today, measured

Both halves of this were run on 2026-08-31 rather than reasoned about.

**The language surface is already identical.** Asking the guest for its own
keys under the native host:

```
$ drt run keys.dlua
call  capabilities  children  crypto  events  exec  fs  monotonic  spawn  sql  time  try
```

which is character for character what Lab's browser REPL offers on `host.<Tab>`.
One `host` library, two hosts, no divergence. That is the doctrine holding.

**The baseline is where they diverge, and it goes both ways.**

| | native (`drt`) | browser (Lab) |
|---|---|---|
| `host.time()` | answers | **fails**: `queue: cannot wait on a queue here -- this code is not yieldable ... on the main thread when the program was not started by a host that resumes it (try --task)` |
| line editing, history, `<Tab>` completion | **absent** — `stdin.lock().lines()` | present |

So neither host is simply ahead. Lab has the terminal conveniences and
cannot answer a baseline call from its REPL; DRT answers every baseline
call and its REPL has no line editor at all — pressing Up feeds a raw
`\x1b` into the line, and because `0x1b` is Lua's *bytecode signature*
byte, the REPL answers `attempt to load a binary chunk (mode is 't')`.

Lab's failure is the one this document exists to name. It is not a missing
connector: the browser has `Date.now()`. It is that the REPL evaluates on a
thread that cannot park, so the queue round-trip underneath `host.time()`
has nowhere to yield to. **A baseline family that cannot be answered from a
host's own REPL is not answered by that host**, whatever the connector
behind it could do — rule 4.

Worth noting where the completion data lives, because it decides who
implements it: `host.<Tab>` is `pairs(host)`, which is a *guest* question.
DRT's REPL is already a guest program driven over two queues (`repl/in`,
`repl/out`); completion is a third message shape on the same seam — prefix
in, candidates out — not a line editor that needs to understand diluvium.
The line editing itself is the host half, and is rustyline natively and
xterm.js in a page. That split is what keeps one REPL from becoming two.

## The trap this replaces

The last long session concluded DRT had no `time.now()` and that the guest
API needed a prelude built on top of it. Half right, and the wrong half is
instructive.

`time.now()` genuinely does not exist. But `time` **is** a library — the
pure calendar one — so typing `time.now()` gets you

```
attempt to call a nil value (field 'now')
```

which reads as a broken runtime rather than a wrong name. The clock is not
on `time` because the clock is a **capability**: it costs a grant, it is
answered by a connector, and it lands in the log so a replay replays the
same moment. Calendar arithmetic is pure and free; asking what time it is
is neither. So it lives on `host`.

The fix was never a prelude. It was saying so out loud, which is what this
document and `examples/hello.dlua` now do.
