# What is next, sized

Written 2026-09-01, at v0.4.0. Everything here was sized by reading the
code, not by estimating from a description, and each entry names the file
and line the estimate rests on so nobody has to re-derive it.

**Three of these corrected a claim made in the request that prompted
them.** Those corrections are kept in place rather than quietly fixed,
because a wrong shared belief about where the cost is will be re-derived by
the next person otherwise.

The first five are independent, unblocked, and about two and a half days
together.

---

## Unblocked, small

### 1. `wall_ms` and `spawns` budgets — ~1 day

`Budget` is `{instructions, memory_kb}` (`drt-config/src/lib.rs:27`). Both
are properties of the guest VM. Neither bounds the two things a swarm
actually spends — wall-clock time and cumulative spawns in a lineage.
`instructions` cannot express wall time at all: a hostcall that parks for
85 seconds consumes none.

`fits_within` and `resolved_against` (`drt-config/src/lib.rs:69-85`) are
already written as a generic `fits(child, parent)` over `Option<u64>`
applied per field, so the attenuation semantics — `None` inherits, a child
may state smaller and never larger — come free for new dimensions.

**Correction worth keeping: this needs no `diluvium` change.** The request
that raised it said "plus `dv_set_budget`'s surface in `diluvium`". Not for
these two. `instructions` and `memory_kb` have to go through
`dv_set_budget` because they are VM properties. `wall_ms` and `spawns` are
host-enforced by construction — the host is the only thing that *can* see
them — so this is pure DRT: no ABI bump, no pin move, no cross-repo
coordination, which is usually the expensive part.

Enforcement has a home already: `DeployHost` carries a tick counter and
`Instant` deadlines, and every slot carries `parent`, so a cumulative
lineage counter is an increment along an existing chain.

Deliberately deferred out of v0.4.0: it changes config parsing, and this
repository's own traps list says to re-run the examples when that happens.
It also adds fields the C host does not have, which is divergence in the
one direction the priority order forbids — so the *shape* wants agreeing
with `dhost.c` before it ships, not after.

### 2. The scheduler — ~½ day, or weeks

Today the only way to run something later is an instance awake on a
timeout, which scales with the number of scheduled things rather than with
work.

**Correction: most of the timer already exists.** `next_deadline`
(`drt/src/start.rs:206-209`) already computes the earliest pending park
deadline across the roster, and the drive loop already sleeps toward it
(`start.rs:326-328`). Two concrete gaps:

- `.min(IDLE_TICK)` caps the sleep at 1 ms, so an idle deployment still
  wakes a thousand times a second however far off the next deadline is.
  Letting the sleep run to the deadline when nothing else needs a tick is
  nearly a one-liner.
- `detached` (`start.rs:154-156`) drops the park — deadline included — when
  an instance hibernates. So a hibernated instance has no timer, which is
  exactly the case this is supposed to serve. The deadline has to outlive
  residency.

The "must not become a supervisor type" constraint is satisfied for free:
this is a timer on a park that already exists, not a new object with a
lifecycle.

**The caveat that changes the estimate by an order of magnitude.** A
deadline in a `HashMap` dies with the process. If scheduled things must
survive a restart, the deadline belongs in the snapshot store, and that is
weeks, not hours. Decide which is being asked for before starting.

Deferred out of v0.4.0 because it touches the drive loop, which is exactly
where v0.4.0's own relay-spins-a-core bug lived. Not the week of a release.

### 3. Idle step cost — ~½ day

`Swarm::step` makes three full passes over `self.slots`
(`drt-swarm/src/swarm.rs:782`, `:797`, `:802`) every step, regardless of
how many instances are alive. An active list alongside the table makes idle
cost proportional to work.

### 4. `kill_subtree` is quadratic — ~½ day

`drt-swarm/src/swarm.rs:564-589` is a fixpoint loop with a full slot scan
inside it, so tearing down a lineage is O(depth × n) — worst case O(n²) on
a deep one. A child index fixes it.

**Correction: this is the quadratic, and it is not on the path people
think.** The claim that prompted this was "a host walking its own roster is
quadratic today". That is not true: `Swarm::find` is O(1), a lookup in
`self.index` maintained on claim and release (`swarm.rs:337-342`, `:355`,
`:367`), and every per-instance accessor — `parent`, `resident`,
`wake_on_message`, `cached_size`, `budget`, `caps`, `holds` — goes through
it. `ids()` plus N accessor calls is O(n). The real quadratic is on the
teardown path, where a swarm that grows a leaf per pattern will meet it.

### 5. Capability flags, wasmtime-style — ~half a day for `--dir`, precedence settled

`drt --dir ./work run app.dlua`, `drt --listen http://127.0.0.1:8080 start` —
grant a capability from the command line instead of writing a config, the way
`wasmtime --dir` does.

This matters more than convenience. With no config only `time` and
`time/monotonic` are wired, so a standalone `.dlua` can compute but cannot do
I/O, and that is what stops a single file from feeling like an app. A `--dir`
flag closes that without weakening the scope model: the flag still names a
*place*, which is exactly what a scope is.

**The mechanism is already sketched.** `assemble()` in `crates/drt/src/main.rs`
mutates a `RootConfig` before wiring — that is what `local_defaults` does — so
flag merging has a natural home right beside it. `Listener` defaults
everything but `scheme` and `address`, so `--listen http://127.0.0.1:8080`
maps to one struct with no other choices to make. The fs half is a
`ConnectorWiring` with a scope, which is the same shape a config writes.

**Worth knowing before starting: the intent is already documented as if it
were built.** `Cli`'s doc comment reads *"Root config file. Flags and env
merge over it into one root object."* Nothing merges. `--config` is the only
flag, and no code reads an environment variable. That is the same shape as
`BUILDINFO`'s `dv_abi`, which release.yml's header promised from v0.1.0 and
did not emit until v0.4.0 — a comment describing a design rather than the
code under it.

**The policy decision, which is the part that is not easy.** DRT has both a
config and flags, so precedence has to be decided, and the obvious answer is
wrong:

- *Flags always win.* wasmtime's model, and coherent there because flags are
  the only config. Here it would mean a command line can widen a config's
  `caps` ceiling — and a ceiling that a flag can raise is not a ceiling.
- *Flags apply only when there is no config.* Safe, trivially explainable,
  and it covers the case that motivated this. It also makes the flag useless
  for the ordinary "run this deployment but point fs somewhere else" edit.
- *Flags merge, but may only narrow.* Consistent with DRT's own attenuation
  rule — a child may state a smaller number, never a larger one — applied to
  the command line as one more layer. A `--dir` naming a directory outside
  the config's fs scope is refused by name, at startup, like every other
  scope mistake.

**Decided (2026-09-01): flags merge but may only narrow.** The effective
scope is the intersection, so there is no precedence rule to remember and no
case convention — "who wins" is "whichever is tighter", and a `--dir` outside
the config's scope is refused by name at startup like every other scope
mistake. The owner's reason for preferring it is the one worth recording:
someone who genuinely wants to widen a capability on code imported from
dollup can download that config and set it explicitly, which is a deliberate
act with a diff, and no path remains that widens by accident.

This is the reason the entry is half a day rather than an hour: the merge has
to run the same attenuation check a spawn does, rather than a
`BTreeMap::insert`.

Worth doing alongside, since it is the same decision: honour the `DRT_*`
environment variables the doc comment implies, or delete the clause. Either
is fine; the current state — promising a merge that does not exist — is not.

---

## Blocked on something else

### 6. Roster introspection in one query — ~½ day of swarm work, gated on the control endpoint

The data is all there behind O(1) accessors; aggregating it into one struct
is half a day. But `drt ps` is a stub that says so
(`drt/src/main.rs`): it reaches a running deployment over the control
endpoint, which SPEC §13a lands with sshd.

So this is not gated on swarm work at all. If the notebook is what is
blocked, **the control endpoint is the dependency to schedule** and the
roster query is a small thing hanging off it.

---

## The browser tier

### 7. A verified browser release — ~1 week minimal, 2-3 weeks useful

**Superseded by `doc/Wasm.md` (2026-09-03).** The sizing below assumed the
JS-host-in-the-middle design; the plan there links the C core into the
browser module instead, measured on this tree, and re-sizes the work as
milestones M1–M7. M1–M4 and M7 landed the same day: the browser artifact
ships from `release.yml`'s `build-web` leg, admitted by the examples gate
running in Chromium (`doc/Browser.md`). Kept for the record; the "what is
already there" list below describes the bridge that M7 retired.

`drt-web` became a second-class citizen, and there is a visible feedback
loop in this repository's own reasoning about why. `ci.yml:80-87` and
`doc/Release.md` both argue — correctly — that wasm32 stays out of the
release matrix because every other leg proves itself by *running* the
artifact and a wasm one cannot be run on the runner. Sound, and
self-reinforcing: it cannot ship because it cannot be verified, and nobody
builds the verification because it does not ship.

**What is already there, which is more than "unwired":**

- `HostBridge` is the JS contract as a Rust trait, and a mock bridge drives
  a **real `Swarm`** under ordinary `cargo test`. The seam was kept honest,
  which is why this is wiring rather than a rewrite.
- `wasm-bindgen` and `js-sys` are already declared under
  `cfg(target_arch = "wasm32")`, with the `default-features = false` trap
  on `drt-swarm` already found and documented in that Cargo.toml.
- `doc/Browser.md` has the whole contract designed: a 16-row export table,
  the panic-safety rule, and the throws / must-not-throw table.
- CI compiles `drt-web` for wasm32 on every push, green.
- The interpreter half exists and works: `diluvium/bindings/js` is a real
  package — `instantiate()`, `Instance`, `Wait`, `Step`, msgpack, a WASI
  shim — and its `instance.integration.mjs` is wired into that repo's CI.

**What is missing:**

| piece | size |
|---|---|
| `#[wasm_bindgen]` export layer | 1-2 days — 16 methods, each a panic-wrapped shim over an existing O(1) `Swarm` accessor |
| Import glue (a JS object bound to `HostBridge`) | 1-2 days |
| Playwright harness and a first real test | ~1 day |
| `BUILDINFO` `profile.web.exports` and the matrix leg | ~½ day |
| Connector/pump layer (the third piece of #31) | ~1 week, and the only one with real unknowns |

**Raise the smoke bar before meeting it.** `doc/Release.md`'s stated
trigger for the wasm32 leg is a **node** smoke step asserting
`abiVersion() === 1`. The one real browser-vs-native divergence this
project has actually observed is in `doc/HostBaseline.md`: Lab's REPL
cannot answer `host.time()` at all, because it evaluates on a thread that
cannot park, so the queue round-trip has nowhere to yield. A node smoke
test will never see that — node's event loop is not the browser's — and it
is precisely the "sorry, that's different in Lab" failure the browser-first
plan existed to prevent. Playwright is not gold-plating the gate here; it
is the only proposed test that catches the failure mode already observed
once.

**Risks to settle before starting, not after:**

1. `diluvium/bindings/js` is `diluvium` v0.1.0 and appears unpublished. A
   browser release has a hard dependency on that package reaching
   consumers, and `doc/Release.md`'s mirror ask covers binaries only.
2. DRT has no JS tooling today — no `package.json`, no node in CI. This
   adds `wasm-bindgen-cli` to the release job and a node step to `ci.yml`.
3. Panic-safety is per-export. It is easy to wrap fifteen of sixteen, and a
   panic crossing the boundary is UB rather than an exception. Worth a test
   that deliberately panics one.
4. `swarm.integration.mjs` in diluvium **has never executed anywhere** —
   its own header says so, because building its module needs the wasi-sdk
   container. That is the *C* swarm's JS route, which `drt-web` replaces,
   so it does not block; but the JS-side swarm path has no prior green run
   to lean on.

---

---

## Recorded, not sized: scopes that decide at call time

Four asks arrived together, and they are **one extension wearing four
faces**. Writing them separately would get them built separately, which is
how a capability model grows four incompatible dialects.

Every scope DRT has today answers **"where"**, once, at startup: `fs` names a
directory, `sql` names a database, `ssh` names a host, `rest` names an origin
allowlist. `ScopeType::validate` runs before the first call and refuses a
malformed scope by name while the operator is still looking at the terminal.

These four all ask a scope to answer **"whether, now"** — a predicate over the
call and its context rather than a place.

- **Rate limits**, per message or per call, with reactive-extensions shapes:
  debounce, throttle, sample.
- **Time of day**, and this one generalises: any capability could carry
  "only between 09:00 and 18:00", not just email.
- **Email (`ssmtp`)** scoped by recipient domain or specific address.
  **Built** — `connectors/ssmtp`, `full` only. It turned out to be the one
  of the four that needed nothing from the extension below: a recipient
  allowlist answers *where*, at startup, exactly as `rest`'s origin
  allowlist does. See the note under its heading.
- **Geofence** — the host supplies a location, the scope evaluates a radius
  around a point or a simple polygon. Explicitly not to be built; recorded so
  the shape below is designed with it in mind rather than retrofitted.

### The good news, from reading the trait

`Connector::call(&self, call, args, scope)` **already receives the scope on
every call**, not once at wiring. So a scope that decides per call needs no
trait change and no new plumbing — the connector simply consults more of it.

Two costs, and they are the whole design:

1. **State.** `Connector` is `Send + Sync` and `call` takes `&self`, so a rate
   limiter's counters need interior mutability. That is ordinary, but it is
   the first time a connector would carry state that outlives a call, and
   where that state lives decides whether a limit is per instance, per
   lineage, or per process. Those are three different products.
2. **Where the refusal happens.** DRT's startup validation is a property
   worth protecting: a bad scope is caught before anything runs. A dynamic
   scope keeps that for its *shape* (a malformed cron expression or polygon
   still fails at startup) but not for its *outcome* — "denied, outside
   permitted hours" can only be said at the call. That is a real change to
   what an operator can learn before starting, and it should be stated
   rather than discovered.

### Two of these are not the same thing, and should not share a mechanism

**Rate limiting as capability policy** ("this app may make 10 `rest/get` per
minute") belongs in the scope. It is an authority question, it attenuates
like any other, and a child asking for a higher rate than its parent is the
same refusal as a child asking for a wider directory.

**Debounce and throttle as stream operators** ("collapse these queue messages")
are not authority at all — they are queue machinery, and diluvium's queues
already carry bounds and a full-queue policy. Putting an Rx operator in a
capability scope would make the scope responsible for delivery semantics,
which is a different concern that happens to share vocabulary.

**Confirmed by the owner (2026-09-01): the Rx naming is borrowed for its
shape, not its layer.** So the ask is the authority half — a rate limit is a
grant, attenuates like a grant, and is refused like a grant. `debounce` and
`throttle` are vocabulary for *how a limit behaves when it is hit* (drop,
delay, collapse), not a request to move stream operators into the capability
model. Worth keeping in the eventual arg names: a scope saying
`{ rate = "10/min", on_exceed = "throttle" }` is authority with a named
behaviour, whereas one saying `debounce(500)` is a queue asking to be
misfiled as a permission.

### `ssmtp` is the cheapest of the four, and it is now built

It is `rest`'s sibling. `connectors/rest`'s scope is an origin allowlist
checked twice — against the URL, then against the resolved address — and an
email scope is the same structure with recipients in place of origins:
allow `@example.com`, or one exact address, and refuse the rest by name.

It also inherits the part of `rest` worth having: an allow entry can carry
operator-supplied `headers` that the connector injects and the guest can
neither set nor read. For SMTP that is the credential and the envelope
sender — the app sends mail without ever holding the password, and cannot
forge the From line. That is the capability model doing its actual job, and
it is the argument for `ssmtp` being a connector rather than something a
program reaches through `rest`.

**Landed as written, and it needed none of the predicate work below.** The
sizing above was right about the shape and wrong about the grouping: a
recipient allowlist answers *where*, once, at startup — the same question
every scope DRT already had — so `ssmtp` was never really a member of the
call-time family. What it did need, and what the sizing did not mention,
was the part that has nothing to do with capabilities: SMTP header
injection, dot-stuffing a body line of `.`, and refusing a scope that would
send AUTH before STARTTLS. Those are three-quarters of the connector.

The other three items below are unaffected and still want the shared
predicate.

### What `ssmtp/send` still cannot say, and which of it is safe

A reply can now name what it answers — `in_reply_to` and `references`
landed as guest-settable because neither routes: a relay reads nothing
from them, a client only threads on them, so the check is on shape and not
on trust. That is the test for every other header a program might want,
and applying it sorts the rest into three piles. Sized against the code:
each is under half a day, and none needs a scope change except the last.

**The connector should supply, and the guest cannot ask for:**

- **`Message-ID`**, minted here as `<random@sender-domain>`, written on the
  wire, and **returned in the reply** beside `recipients`. Without it the
  relay assigns one the program never sees, so a program can answer a
  thread but cannot recognise an answer to its own mail, nor thread its
  own follow-up under its own first message. Entropy is `getrandom`,
  already in the tree behind `crypto`. One caveat to document rather than
  hide: SES rewrites `Message-ID` to its own, so under SES the returned id
  is the one the guest sent and not the one the recipient will quote — the
  honest fix there is to also return the relay's `250` text, which carries
  SES's id.
- **`Date`**. RFC 5322 requires it; relays add one when it is missing, but
  a message that arrives at a filter without it scores as spam
  (SpamAssassin's `MISSING_DATE`). Host-side clock, not a guest read.

**The guest may set, because the value is an enum or is checked by the
scope that already exists:**

- **`Auto-Submitted`** (RFC 3834): `auto-generated` or `auto-replied`. The
  one that matters for a program that answers mail — a responder on the
  other end must not answer an `auto-replied` message and will answer a
  plain one, and two programs answering each other is a loop with no
  floor. A fixed vocabulary, so nothing to inject.
- **`Cc`**, and `Bcc` if wanted: recipients under the same `allow` check
  and the same `MAX_RECIPIENTS` bound as `to`, since a `Cc` is a `RCPT TO`
  with a different header. Routing, but routing the scope already
  governs.

**The scope's, never the guest's:**

- **`Reply-To`**. It is where answers go, which is the forgery `from` being
  scope-only exists to prevent: a program that could set it sends mail
  from the deployment's address with replies diverted to any address it
  likes. An operator-side `reply_to` in the scope, beside `from`.

**Not a free-form `headers` map**, whatever the denylist. A list of names
a guest may not set is a list that is one RFC behind; the named-argument
shape above is an allowlist, and every entry on it carries its own check.

### If this is built, build the predicate once

The shape that serves all four: a scope may carry an optional `when` — a
predicate over call context (clock, call count, location) — evaluated by a
shared helper rather than reimplemented per connector. Then time-of-day is
one predicate, rate limit is another, geofence is a third that nobody has to
build yet, and a connector author gets them by declaring a scope rather than
by writing them.

The alternative — each connector growing its own `hours` field — is four
implementations, four spellings, and no way to say "this grant is
business-hours-only" about a capability whose author did not think of it.

## Not in this list

**FM-2's upstream fix** has its own brief: `doc/FM-2-Upstream.md`. DRT's
mitigation shipped in v0.4.0 and the real repair is diluvium's.

**The `v*` tag ruleset** that 403'd v0.3.1 is unchanged and is not an
engineering task: an admin clears the rule, or a human account publishes
from the Releases page. See `doc/Release.md`.

**Token budgets** are in neither list because they are not a sizing
question. With a generic `rest` connector the response body lands in the
guest, so the count is self-reported by the thing being budgeted, and no
amount of implementation makes that enforceable. The two honest options —
host-side call/byte metering at the queue layer (SPEC §7 designs it and
nothing builds it, a few days) and a purpose-shaped `llm` connector that
owns the token meter — compose, and the second buys enforceability at the
price of the host knowing one API's response shape.

Worth noting v0.4.0 moved this slightly: the `rest` connector's scope can
carry operator-injected headers, so the host already holds the credential
and already sees the response bytes. A token meter could live behind that
same scope without a new capability family — it would still have to parse
one API's usage block, but it would not need a second grant.
