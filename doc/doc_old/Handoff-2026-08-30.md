# DRT handoff

Written 2026-08-30, at `5e50604` on `claude/diluvium-runtime-spec-gq95of`.
Working tree clean, everything pushed.

Read this before touching anything. It exists because a long working
session found more than it could act on, and the findings are worth more
than the code that came with them.

---

## Status in one paragraph

DRT works. It runs sandboxed Lua under a capability model, serves
fetchpoints today, speaks the C host's config dialect, and ships as a
~3 MB static binary with no runtime dependencies. 146 tests green. What
it does *not* yet have is a first hour that a stranger can survive: the
nouns are inverted relative to how users will read them, and `drt start`
with no arguments prints a schema fragment and exits 1. None of that is
structural. All of it is unfinished surface.

*(The "smallest useful program is fifteen lines of queue boilerplate"
line that stood here was wrong — `print(host.time())` has always worked.
See decision 4, which is now closed.)*

## Priorities

1. **Compatibility with the C host comes first.** DRT exists to run the
   same deployments; a divergence there outranks anything below.
2. **Anything users touch is held to a higher standard** than internal
   surfaces. This is why several obvious "fixes" below are deliberately
   *not* done yet.

---

## What works right now, verified by running it

Not inferred from tests — actually executed on this tree.

```sh
# cold, no config, no install beyond chmod +x
echo "print('hello')" > hello.dlua
drt run hello.dlua                              # -> hello

# the capability model, in two commands
drt run examples/hello.dlua                     # everything denied but time
drt run --config examples/deployment.json       # fs granted, escape refused
```

The second pair is the best demo in the repo. Same program, two
configs:

```
fs/read note.txt: ok hello from the workspace
fs/read escape:   error '../../etc/passwd' resolves outside the granted scope
sql/query:        denied 'sql/query' is outside this instance's grants
```

The denial strings are the best-written text in the product. Lean on
them; don't rewrite them.

Also working and proven end to end: `drt start` with a real config,
the rendezvous relay (presence, metering, arbitration), STUN, and SSH
over WSS through `drt tunnel` — a real `ssh` session, 37 bytes metered,
rehearsed against the release artifact at v0.3.0.

## Binaries

Three platforms built, smoke-tested, and uploaded on **workflow run
33204025600** (`drt-linux_static_x86_64`, `drt-darwin_arm64`,
`drt-darwin_x86_64`). They **expire 2026-09-27**. Built from `86e71c1`,
which is v0.3.1 minus the two commits after it.

You do not need a release to put DRT in someone's hands. Download,
`chmod +x`, go.

---

## Open decisions — do not re-litigate the constraints

Each of these was argued to a constraint set and then deliberately left
undecided. The constraints are settled; the answers are not.

### 1. The naming inversion

Users will call **what we call a deployment** their *program* ("I wrote
a program, I run it"), and will read **what we call a program** (the
`.dlua`) as a *library* or *script* — the instance isolation does not
register as a boundary to someone who just wrote a file.

This propagates into every doc and every error string, which is why the
first-run copy below is blocked behind it.

Sketch, offered as a starting point and not an argument: **service** for
what you deploy, **program** stays the file. "A service is config plus a
program." `drt start` starts a service; `drt run program.dlua` still
reads right.

Cost: a rename, mechanical, and cheap while the surface is still
small. Cheapest class of problem in this document.

### 2. `drt start` with no arguments

Currently prints a JSON schema fragment, a `SPEC.md §5` reference, and a
pointer to a second tool, then exits 1. Every one of those is wrong.

Constraints established, all of them binding:

- **It cannot do nothing**, and a message alone is not enough.
- **It must not bind a port.** A welcome page that serves HTTP makes the
  browser a second-class citizen on day one — precisely the
  guide-divergence problem the whole Lab-first plan existed to avoid. A
  drafted HTML welcome deployment was killed for exactly this reason.
- **It must not drop users into the current REPL** (see below).
- No schema, no `SPEC.md`, no second-tool nag in a path people hit while
  fumbling.

Surfaces that survive the browser constraint: stdio, and the REPL's
"line in, text out over two queues" contract.

**Blocked on decision 1** — do not write this copy until the nouns are
settled, or it gets written three times.

### 3. dollup

Constraint, from the owner: **crucial on day one for distribution**, and
it must **survive an air-gapped install without nagging** — DRT will run
where dollup is not permitted. So: mentioned once, somewhere people go
looking; never in a hot path, never repeated, never a prerequisite to a
first run. The removed `drt start` pointer violated all three.

### 4. The guest-API prelude — CLOSED, and it was already built

**Resolved 2026-08-31. Do not build the prelude. It exists, upstream, in
the C core, and building a second one on top breaks the first.**

The original entry, kept because the half that was right is worth keeping:

> There is **no `time.now()`**. I wrote `time.now()` twice while working
> inside this codebase all day. Anyone new will do the same and get
> `attempt to call a nil value (field 'now')`, which reads as "DRT is
> broken," not "you weren't granted that."

That diagnosis is exactly right. The proposed fix — ship an `ask()` helper
as a prelude so a first program is `print(host.time())` — was not needed:
**`print(host.time())` already works today, on an unmodified DRT.** Run it.

`src/dhostlib.h` in the embedded diluvium is the `host` guest library, and
its header states this document's own rationale nearly verbatim ("A
connector call used to take five lines of ceremony ... the ceremony was
never the design"). It provides `host.time`, `host.monotonic`, `host.call`,
`host.try`, `host.fs.*`, `host.crypto.*`, `host.sql.*`, `host.exec.*` and
`host.spawn`, and DRT has been linking it the whole time —
`crates/drt/tests/host_lua.rs` calls `host.fs.write` and
`crates/drt/tests/stun.rs` calls `host.call`, and both have been green
since they were written.

**How this went unnoticed, and the trap to avoid repeating.** `time` *is* a
library — the pure calendar one — so `time.now()` fails with "field 'now'"
rather than "time is nil", and that reads as a gap in a library you already
found rather than a signpost to a different one. Meanwhile
`examples/hello.dlua` demonstrated the hand-rolled queue pair, which looked
like the guest API rather than like the substrate underneath it. Two pieces
of evidence agreeing, and neither of them the actual API.

**The prelude was implemented before this was caught, and the test suite
caught it.** A composed-source prelude defining `host = {}` destroys the
real `host` table; `a_cap6_shaped_deployment_runs_from_its_host_lua` failed
with `attempt to index a nil value (field 'fs')`. Reverted. The lesson is
cheap to state: before adding to the guest API, read
`types/guest.lua` and `src/dhostlib.h` in the embedded diluvium checkout —
that is where the guest API is defined, not in this repository.

What actually shipped instead, which is what the barrier really needed:

- `doc/HostBaseline.md` — the three families every DRT host must answer
  (`time`, `time/monotonic`, `crypto/random`) and the four rules that make
  an absent family safe. This is the owner's ask ("a standard set of
  functions any DRT host needs to either provide or stub"), and it is what
  makes `drt-web` checkable rather than merely unfinished.
- `examples/hello.dlua` rewritten against `host.*`; the old fifteen-line
  version kept as `examples/by-hand.dlua`, framed as the substrate.
- A **Writing a program** section in the README that names the library and
  explains the `time.now()` trap in place, so the next person meets the
  answer where they hit the question.


---

## Known broken / incomplete

**The REPL is bare.** No line editor anywhere in the tree — the host
half is literally `stdin.lock().lines()`. No history, no ctrl-arrow, no
autocomplete; arrow keys emit a literal `^[[A` into the line because the
tty is in cooked mode. None of the conveniences from earlier diluvium
REPLs were ported. It *is* architecturally sound: a sealed guest
(`unsafe_stdlib: false`), not a privileged `lua_state`, driven by a host
loop that pumps hostcalls the way `drt run` does. Line editing belongs
in the host half — rustyline natively, xterm.js in a browser — which is
the split `repl.rs` was already built around. Do not put users in front
of it as it stands.

**drt-web is unwired.** 343 lines: `HostBridge` (the JS contract as a
Rust trait), an engine and host written against it, and a mock bridge
driving a real `Swarm` under ordinary `cargo test`. Missing: any
`wasm_bindgen` (so no exports in, no glue out) and the connector/pump
layer. That is task #31 — **wiring, not architecture.** The swarm port
was kept behind a seam that does not assume a native host.

The owner's concern here is legitimate and recorded: browser was meant
to be *first*, on the theory that starting in the most constrained
environment and building outward avoids "sorry, that's different in
Lab." It slipped. The demo it cost — SSH into your browser via a
fetchpoint — is unwired, not architecturally lost. Native-only surfaces
that genuinely do not cross: `listen.rs` (thread-per-connection blocking
sockets) and tunnel/relay/stun (tokio, native sockets).

**FM-2 is named, mitigated here, open upstream.** The `host_lua` SIGSEGV
is a data race on diluvium's process-global continuation registries
(`diluvium_shim_addcont`, `src/dshim.c`), hit when two threads call
`dv_new` at once. A symbolized core on 2026-08-31 caught three threads
inside `diluvium_openlibs`, one faulting in `strcmp`. DRT now serialises
instance creation behind a mutex in `drt-swarm/src/engine.rs`, which closes
it here; the real fix is upstream. Shipped exposure was nil either way —
DRT creates instances only on the drive-loop thread — so this was always a
test-harness crash for us. See `doc/Failure-Modes.md` and
`doc/FM-2-Upstream.md`, the latter being the brief to hand whoever picks it
up in the diluvium repo.

The lesson worth carrying past this bug: `addcont` stops writing once every
name is registered, so the window is cold start and nothing else. Four days
of probes looped a warm process and proved nothing. Before you loop a
binary to reproduce a crash, ask whether the thing you suspect can still
fail after its first second of life.

**v0.3.1 is unreleased.** See below.

---

## The release blocker (settled, needs a PR)

`v0.3.1` never published. The workflow ran green through tests and all
three builds; only "Create release" failed:

```
GitHub release failed with status: 403
{"message":"Resource not accessible by integration"}
```

Diagnosis, after checking everything else: **it is not permissions.**
Repo workflow permissions are read/write; org-level is read/write with
no policies. A manual `git push origin v0.3.1` from a session with write
access **got the same 403**, which rules out the Actions token
specifically and points at a **repo ruleset or tag protection rule on
`v*`** — the action creates the tag as part of creating the release.

**Narrowed 2026-08-31, and it holds.** The evidence now has a clean
before/after that the original diagnosis did not:

- Run **33184812461** published **v0.3.0** at 15:26 UTC on 2026-08-28.
  Green, tag created, six binaries plus `BUILDINFO.txt` and
  `SHA256SUMS.txt` on the Releases page today.
- Run **33204025600** failed on **v0.3.1** at 19:34 UTC the same day —
  four hours later, on the *only* failing job, at the *only* failing
  step, `Create release`.
- `git diff 57d2399 86e71c1 -- .github/workflows/release.yml` touches
  **nothing but the BUILDINFO block**. Both `permissions:` blocks are
  byte-identical (`contents: read` at the top, `contents: write` on
  `publish`), and the `softprops/action-gh-release@v2` step is unchanged.

So the same workflow, with the same permissions, making the same API
call, succeeded and then four hours later did not. Nothing in this
repository changed in between, which leaves a repo or org **ruleset /
tag protection on `v*` added in that window** as the explanation, exactly
as suspected — now with the "it used to work" half nailed down rather
than assumed.

One consequence worth knowing: `Resource not accessible by integration`
is the *GitHub App token* refusal. A release created by a human account
from the Releases page is not an integration and is not subject to it, so
**the owner can publish v0.3.1 by hand right now** without touching any
rule. Adjusting the ruleset is the fix that makes the workflow work
again; the manual release is the fix that makes v0.3.1 exist today.

The owner's call: **do not spend more time on it; open a PR and they
will merge.** No tag exists, so a retry after the rule is adjusted is
clean.

---

## Traps — things that cost real time here

- **`time.now()` does not exist, and the error misleads.** `time` is the
  pure *calendar* library, so `time.now()` answers "attempt to call a nil
  value (field 'now')" rather than naming the real one. The clock is
  `host.time()`, on the `host` library — see decision 4 and
  `doc/HostBaseline.md`.
- **The guest API is defined upstream, not here.** `types/guest.lua` and
  `src/dhostlib.h` in the embedded diluvium checkout are the source of
  truth for what a program can call. Reading only this repository will
  make you build something that already exists.
- **The grant field is `capability`, not `cap`.**
- **`access` is `"read"` or `"readwrite"`, never `"readonly"`.** This
  changed in v0.3.1 to match the C host (`dhost.c:709`, `:1010`), where
  taking `"readonly"` and refusing `"read"` would crash-loop an existing
  discofetch config. The fix missed `examples/deployment.json`, so the
  one config file a new user copies **did not load** — found by running
  it, not reading it, and fixed in `5e50604`. **When you change config
  parsing, run the examples.**
- **`--config` works before or after the subcommand** (clap global).
  Both invocations in `examples/hello.dlua`'s header are valid.
- **A green `segv-probe` is only meaningful because of two guards added
  in `0853e0a`** — the binary must exist and be executable, and an
  all-rounds-failed run is an error. Before those, an empty `$BIN` would
  have reported 800 clean passes. Check what a probe actually ran.
- **This dev box hits 100% disk during full builds.** `CARGO_INCREMENTAL=0`
  and clear `target/` when writes start failing. A "tests passed" line
  once turned out to be an echo masking a disk failure; the 146-green
  figure is from a clean re-run.

---

## What I would do next, in order

1. **Have someone unfamiliar with the codebase run it** and watch where
   they stall. This is now the highest-value item by a distance: decision
   4 was closed by *reading* rather than building, and the only reason it
   stayed open for a day is that nobody unfamiliar had tried the thing.
   Releases are downloadable today — see the README's Installing section.
2. **Settle the nouns** (decision 1), then write the `drt start` copy
   (decision 2) once, against the settled names.
3. **Publish v0.3.1.** The owner can create the release by hand from the
   Releases page; the App-token 403 does not apply to a human account.
   Adjusting the ruleset is the separate fix that makes the workflow work.
4. Leave drt-web and the REPL conveniences alone unless something forces
   them — though `doc/HostBaseline.md` now says what drt-web has to answer,
   which is the part that was missing. FM-2 needs no more work *here*: the
   mitigation is landed, and what remains is the upstream fix, briefed in
   `doc/FM-2-Upstream.md`.

The instinct to fix the first-run message *first* is still wrong: decision
1 rewrites it anyway.
