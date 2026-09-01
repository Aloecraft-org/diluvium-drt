# DRT's reply to the 0.5.0 ask

**Written 2026-09-01, against `v0.4.0rc1`.** Answers
discofetch's *"What discofetch needs from DRT — the 0.5.0 ask"* (1 Sep,
written against `d629341`) and its §2.2 addendum.

Same convention as the ask: `verified` means I ran or read it and name the
file and line, `reproduced` means I built the failing case and watched it
fail, `open` means I have not established it.

**Short version.** The ask is right about almost everything, and the two
items it marked `reported` both reproduce. Three corrections change the
plan rather than the prose: §1.2 is not DRT's bug and build12 does not fix
it, §1.3's preferred fix is already implemented and the real defect is a
different one, and §2's TLS decision is bigger than a flag. Sequencing
holds otherwise.

---

## surface block

1. What reproduced, and what it changes.
2. Corrections that move work between repositories.
3. The decisions I need from you before starting.
4. Assessment of the doc as a document.
5. Sizing, and what I need to do it.

---

## 1. What reproduced

### §1.1 budget attenuation — `verified`, agreed, no correction

`Budget::fits_within` (`crates/drt-config/src/lib.rs:69`) and
`InstanceConfig::check_attenuation` (`:114`) have no caller outside their
own tests. The spawn path takes `field_budget(request)` and assigns it
(`crates/drt-swarm/src/swarm.rs:955`) with no comparison against the
parent. Two independent reads, same conclusion, nothing to argue about.

Agreed it goes first, and agreed the refusal should be a reply and not a
fault — to a program a budget refusal is the same kind of event as a
capability refusal, and `08-spawn-and-hibernation` should show it.

### §1.2 the pcall escape — `reproduced`, and worse than reported

Two lines of Lua:

```lua
pcall(function() while true do end end)
while true do end
```

Under `budget.instructions = 1000000`: the first loop trips at ~250k
steps, `pcall` catches it as an ordinary error, and the second loop runs
**without limit** — killed at 20 s, `drt run` had not returned. It is not
that the budget is advisory. **The budget is switched off by its own first
firing**, permanently, for the life of the instance. And `drt run` exits 0
when the program then finishes normally, so nothing above it can tell.

For discofetch this is worse than a tenancy hole: it is an
unauthenticated way for a fetchpoint program to pin a core forever.

**The correction that matters: this is not DRT's code.** `src/dv.c:219`,
in the instruction hook:

```c
inst->exceeded = 1;
lua_sethook(L, NULL, 0, 0);   /* once is enough; the error is on its way */
luaL_error(L, "instruction budget of %I exceeded", ...);
```

The hook disarms itself and raises a catchable Lua error. `pcall` catches
it; the hook is gone; nothing re-arms it. **This is byte-identical in
diluvium `main` today** — I fetched build12 and diffed it — so the pin
bump does not fix it and neither would waiting.

It also cannot be fixed from the host side, and I want to be precise
about why, because the obvious mitigation looks like it works: `dv.h`
exposes `dv_exceeded()` and the safe crate exposes `Instance::exceeded()`,
so DRT can refuse to resume an instance that has tripped. That bounds
nothing here. A CPU-bound loop never returns to the host, so there is no
resume to refuse. The enforcement has to be *inside the hook*.

**The fix is small and belongs upstream:** on `exceeded`, do not clear the
hook — leave it armed so it fires again `DV_HOOK_STEP` instructions later.
Then any `pcall` handler is itself interrupted, and the error becomes
uncatchable in practice without needing an uncatchable error mechanism
Lua does not have. Second, `dv_resume` should refuse to start on an
instance whose `exceeded` is set, so a yield-and-resume guest cannot walk
past it either.

**Cheap half DRT can do meanwhile — done, in v0.4.0rc1.** `drt run` no
longer exits 0 when `exceeded()` is true. That does not restore the bound
(the program has already run) but it stops the escape being *silent*,
which is what made it dangerous to a supervisor.

Worth being precise about who can already see this, since it is the first
question anyone asks. **A host can detect it; a guest cannot.** `dv.h`
exposes `dv_exceeded()`, the safe crate exposes `Instance::exceeded()`,
the `Engine` trait carries it (`drt-swarm/src/engine.rs:261`), and
`drt start` has always read it to classify a stop as
`exceeded`/`faulted`/`exited` (`swarm.rs:829`) — so a supervisor program
watching a child's stop event has had this information all along. `drt run`
was the one place that threw it away. There is no guest-side query, and
there should not be: a program that could ask how much budget it had left
would be a program that could schedule around it.

### §1.3 SQL — `reproduced`, and the ask's preference order is aimed at the wrong option

Reproduced exactly as described: `begin` → ok, insert → ok, in-process
`select` sees the row, process exits, row gone.

But the diagnosis behind it is wrong in a way that changes the fix.
`begin`/`commit`/`rollback` **are implemented and honoured** — they pass
through to SQLite on a held connection:

| sequence | second process sees |
|---|---|
| `begin`, insert, `commit` | the row |
| `begin`, insert, *(no commit)* | nothing |
| insert, no `begin` at all | the row |

So option 1 in your preference order is already done, and option 2 —
refusing `begin` by name — would **remove working functionality**. The
connector's own header says "autocommit only", and that comment is what is
wrong, not the code.

The real defect is narrower: **an open transaction at process exit is
discarded silently.** No error, no warning, exit 0. That is correct SQLite
behaviour on a dropped connection and it is exactly the shape discofetch
cannot defend against, because every layer above believes the `ok`.

**The decision I need is in §3.1.** It is a real fork and it is yours,
because it is your durable tier.

### §1.4's two notes — both accepted

`Failure-Modes.md` gets the no-reactor pattern as a named class rather
than two incidents. You are right that it is one bug found twice, and the
third is cheaper to prevent: the shape is `pollster::block_on` with no
tokio reactor under it (`run.rs:67`, `repl.rs:73`, `pump.rs:47`), and any
connector that touches `tokio::time` or `tokio::net` dies on it. Every
connector that can reach a socket now carries the owned-runtime fallback;
what is missing is the *test* that would have caught it, which is why
`connectors/ssh` having zero tests is on my list and not just yours.

---

## 2. Corrections that move work between repositories

Summarised because they change who does what, not just what is true.

| item | ask says | actually |
|---|---|---|
| §1.2 pcall escape | DRT, `reported` | **diluvium**, `src/dv.c:219`, reproduced, not fixed in build12 |
| §1.3 SQL | implement transactions, or refuse `begin` | transactions already work; the defect is silent rollback at exit |
| `host.exec` | unreproduced; README appears right | both readings correct — see below; README right, unedited |
| FM-2 | (not in the ask) | fixed upstream in build12; the rc still pins `f137b30`, so DRT's mutex stays |

**On `host.exec`, since the ask asked to have it read closely.** Your grep
is correct: there is no `exec` in `crates/drt-swarm/` or
`crates/drt-hostcall/`. The earlier finding is also correct:
`print(host.exec ~= nil)` prints `true`. Both, because `host.exec` is not
DRT's — it is in diluvium's guest library (`src/dhostlib.c`), compiled
into the binary. A grep of DRT's Rust could not have found it and its
absence there proves nothing. `host.exec.run(...)` answers `denied: no
connector is wired for 'exec/run' in this process`, which is what the
examples README says. **The README was not edited toward a false
statement**; it is accurate as written and I have left it alone.

---

## 3. Decisions — all three answered

Answered 2026-09-01, recorded here rather than in a thread. Each was
discofetch's to make because each trades something that is theirs.

### 3.1 SQL: what should an open transaction at exit do?

- **(a) Refuse to exit clean.** An instance stopping with a transaction
  open is an error, named, non-zero. Loudest, and the only option under
  which "every layer above believes the reply" stops being a hazard.
- **(b) Commit on clean exit, roll back on fault.** Most forgiving,
  matches what a program that forgot `commit` probably meant, and is a
  guess about intent that will be wrong for somebody.
- **(c) Leave the behaviour, fix the docs, add a diagnostic.** Cheapest;
  keeps SQLite's semantics exactly; still loses the row.

I would take **(a)**. Your durable tier is the argument: a lost write you
are told about is a bug report, and a lost write you are not told about is
a data-integrity incident that surfaces weeks later. (b) is the one I
would push back on hardest — a runtime that guesses at commit intent is a
worse promise than one that refuses.

**Decided: (a).** An instance stopping with a transaction open is a named
error and a non-zero exit. Note what this does *not* change, since the ask
was written believing otherwise: `begin`, `commit` and `rollback` all stay,
because they already work. The connector's "autocommit only" header comment
is what gets deleted.

### 3.2 netcheck's TLS decision (§2.2a) — the profile question, not the flag

You are right that this one is mine to make and right that it touches the
profile list, which is why I want it on the record before the work rather
than during. `stun = ["dep:tokio", "dep:ego_transport"]` is a UDP client
and nothing else; `--reflect` needs HTTP and TLS.

**My answer: a new `netcheck` feature, separate from `stun`, in `full`
only.** Reasons in order: `rest` already links a TLS stack and is already
`full`-only for exactly this argument, so the marginal artifact cost in
`full` is a client and not a stack; `stun` stays cheap, which matters
because the STUN *server* is the thing a small deployment runs and it has
no business carrying TLS; and the verb and the server are genuinely
different products that happen to share a name.

Consequence you should price: `slim` loses `drt netcheck` entirely,
including the verdict tree it can compute today with no network. If that
is unacceptable, say so and I will split further — verdict tree in `slim`,
measurement behind `full` — but that is a third feature flag for a
diagnostic and I would rather not.

**Decided: as proposed.** A `netcheck` feature, `full` only, `stun` left
cheap. `slim` loses the verdict, and that is priced and accepted. It also
means the connector list in `BUILDINFO` changes again, which is the
argument for 0.5.0 the ask already makes.

### 3.3 `--port`'s meaning (§2.2c)

Agreed it is ambiguous and agreed with your suggestion. `--port N` means
*"probe the service already listening on N"* and binds nothing. If the
"can anything reach me at all" test is wanted, it is a different flag with
a different name that says out loud that it binds a listener.

**Decided: as proposed**, and with the second flag confirmed as future
work rather than something that exists. To be unambiguous, because the ask
reads as though one of them might already be there: **neither flag exists
today, and netcheck opens no inbound socket at all.** `--port N` (probe a
service already listening, bind nothing) and a separately named flag that
does bind a scratch listener are both §2 work. The naming rule is the
point of the split — nobody should discover after the fact that a
diagnostic bound a socket, so the flag that binds one has to say so.

**Already done for the rc:** the renderer no longer advertises a flag the
binary does not accept. `inbound not tested (no --port given)` is now
`inbound not measured (no inbound test in this build)`. That closes your
acceptance item "no flag is advertised that the binary does not accept"
without waiting for §2.

---

## 4. The doc, as a document

Assessment, since it was asked for.

**It is the most useful thing anyone has handed this repo.** Three
properties, and I would keep all three in whatever comes next:

- **Every claim carries how it was established.** `verified` / `reported`
  / `unreproduced` is doing real work — it is why §1.2 and §1.3 got
  reproduced this session instead of planned around, and it is why the
  `host.exec` correction landed as a question rather than a reversal.
- **§5, "what we are explicitly not asking for."** Naming the things not
  to build is worth more than most of what is asked for. `turn_credential`
  would have grown a consumer eventually if nobody had written that line.
- **§0's two premises bound the whole thing.** "The relay is the floor and
  the floor is acceptable" is what makes §3 sizable at all; without it
  every punch decision reopens.

**Where it is wrong, and it is worth saying why the errors clustered.**
Both `reported` items came from a session summary rather than from code,
and both were *directionally* right and *specifically* wrong — §1.2 blamed
the wrong repository, §1.3 asked for a feature that already exists. The
addendum's `verified` items, by contrast, are all correct. The lesson is
the one the doc already half-states: a `reported` claim should be treated
as a lead, and your own instinct to flag them before sending was right.

**Two structural notes.**

- §7's sequencing survives all of this, with one change: **§1.2 should be
  filed against diluvium immediately, in parallel**, because it is not on
  DRT's critical path and it has lead time — same argument the doc makes
  for the half-close in §3.6.
- The §2.2 addendum is better than §2.2 and should replace it rather than
  sit after it. §2.2b in particular — nothing in the tree can pin an
  outbound source port — is the finding that resizes §2, and it is
  currently three screens below the section it invalidates.

**One thing I would push back on.** §6's acceptance list is good but
`"discofetch's 833 checks pass against the 0.5.0 artifact"` is the only
line on it that DRT cannot run. Everything else I can turn into a gate
here. If those 833 are meant to be the backstop, DRT needs either a way to
run them or an agreement that a red battery is a discofetch-side finding
delivered after the fact — otherwise it is an acceptance criterion in a
document neither side can check before shipping.

---

## 5. Sizing, and what I need

Against the corrected picture.

| section | where | size | blocked on |
|---|---|---|---|
| §1.1 attenuation | DRT | small — call site, refusal, tests, example | nothing |
| §1.2 pcall escape | **diluvium** | small upstream; exit-code half is small here | a diluvium session |
| §1.3 SQL | DRT | small once decided | §3.1 |
| §1.4 tests + Failure-Modes | DRT | small | nothing |
| §2.1/§2.2 address + `--port` | DRT | medium | §3.2, and stun1/stun2 up |
| §2.2b TCP EIM (`socket2`) | DRT | medium, separable, defer | nothing — but it decides nothing, so it goes last |
| §2.3 config dialects | DRT | small | nothing |
| §3 the punch | DRT + ego-transport | large, and correctly last | §2's measurement |

**What I need from you, concretely:**

1. **The three decisions in §3.** §3.1 blocks §1.3, §3.2 blocks §2.
2. **stun1 and stun2 on separate addresses.** Everything in §2 waits on
   this and it is the highest-value item on your list. `doc/Verification.md`
   §1.1 says what to run once they are up.
3. **A diluvium session for §1.2**, run in that repo. The brief is §1.2
   above; it is short enough to paste. Doing it here would mean vendoring
   a patch over the pin, which I would rather not do to a release
   candidate.
4. **The runs in `doc/Verification.md`.** §1.1 (netcheck on three real
   networks) and §3 (the parked-two-minutes tunnel case) are the two that
   would change what I build rather than confirm it.

**On ultracode.** Not for §1. Those are three small, precise edits with
clear success criteria and adversarial verification would mostly re-derive
what is already reproduced above — I would rather spend the turn writing
the tests. **Yes for §2 once it is unblocked**, where the fan-out earns
it: the reflect fetch, the token passthrough, the rate-limit-vs-refused
distinction and the v6 inbound upgrade are four independent surfaces that
each want their own adversarial read, and §2.2d's "a 429 rendering as
'port closed' is a confidently wrong answer" is exactly the class of bug a
verify pass catches and a single write does not. **Definitely for §3**, if
it is built.

So: hold the budget, and I will ask when §2 starts.
