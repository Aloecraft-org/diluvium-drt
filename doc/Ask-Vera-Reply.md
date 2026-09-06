# DRT's reply to Vera's asks

**Written 2026-09-06, against the v0.5.0rc4 branch (`claude/v0-5-0rc2-release-p9v3a8`).**
Answers `vera:doc/DRT_ASKS.md` as it stood on
`claude/diluvium-swarm-ai-planning-7n25kn`, which re-read itself against
rc3 (`1145045`) before this was written.

Same convention as `doc/Ask-0.5.0-Reply.md`: `landed` names where and the
test that holds it; `open` says what it would take; a correction is kept
in place. Where an ask's own text is more precise than a summary could be,
this points at it rather than restating it.

**Short version.** Seven asks landed in rc4 (§14, §15, §18, §20, §22, §23's
first half, and §21's fix from rc3), and three earlier ones (§5, §17, §19)
were already in. The largest asks — per-instance scopes (§1), blob handles
(§4), delivery between instances (§13), the lifecycle split (§24) — are
open and sized honestly below. **And §21's own last paragraph is the most
important thing in this document**: the three modes that fail on rc3 are
not, as it supposes, because `rest` stopped blocking. It had not. It has
now.

---

## surface block

1. §21, first, because it changes what the suite is measuring.
2. Landed in rc4.
3. Landed before rc4, confirmed.
4. Open, sized.
5. Corrections to the asks, kept in place.

---

## 1. §21 — the three failing modes, and what they are not

The ask's closing paragraph: rc3 inverts the drive loop, `up`, `xmpp` and
`spend` fail on it where rc2 passes, and *"not yet established whether
those are regressions or Vera's own assumptions about `rest` blocking the
drive loop — the four ordered deadlines exist because it blocks, and if it
no longer does, several of this deployment's numbers are answers to a
question that has changed."*

**On rc3 `rest` still blocks the drive loop.** `Command::Start` calls
`start::start` under no tokio runtime (`cli.rs:598`), so in every
tokio-backed connector `Handle::try_current()` fails and the call takes
`own_runtime().block_on(work)` on the drive thread — v0.4.2's behaviour
exactly, under notes that said otherwise. Measured rather than read: a
child's three-second `rest/get` held its parent's next hostcall for
**3004 ms** on rc3; 1 ms in a control with no call; 1 ms with a runtime
entered, the child's call still completing.

So on rc3 the four ordered deadlines were still answers to the question
they were written for, and the three failures are something else — most
likely real regressions in rc3's drive-loop inversion, and worth a bisect
between rc2 and rc3 that this repository cannot run, since it has no
Vera. Please file what fails with the reproduction; it will be looked at
first.

**On rc4 `rest`, `ssh` and `ssmtp` park.** `run`, `repl` and `start`
enter one ambient runtime before their first tick
(`crates/drt/src/runtime.rs`; `crates/drt/tests/start.rs::parking` is the
measurement kept as a gate, and fails at 604 ms with the entry removed).
So the suite's assumptions change *at rc4*, not at rc3: rc4 is the first
candidate where "if `rest` no longer stalls the process" is true, and the
day of work the ask sets aside to accept that is a day to spend on rc4.
`exec` still holds the loop — its body is `std::process`, with no future
to park — and its doc now says so; `spawn_blocking` is the next change.

---

## 2. Landed in rc4

### §14 `capabilities/list` — `landed`

Answered by the dispatcher, which is the one place holding both halves —
what is wired and what this instance may reach — in the C host's shape,
`{name, kind, owner, granted, visibility}`, plus the menu itself. Gated on
`host:capabilities/list` exactly as `dhost.c`'s `conn_capabilities` gates
it, so an auditor granted that and nothing else can report a swarm's
reach without any of it. `granted` is asked the way a call asks, and a
family held narrowly (`host:fs/read`) is reported as reached. `owner` is
nil and `visibility` is `public` because DRT has no plugins to own a
family and no visibility policy yet; the fields are there so a program
written against the C reads the same map.
`crates/drt-connector/src/lib.rs`,
`capabilities_list_names_every_wired_family_and_whether_it_is_held`.

### §15 `journal_mode` on the `sql` scope — `landed`

Applied on open and **read back**: SQLite answers the pragma with the
mode it is in, so a read-only connection that cannot convert a database,
or a filesystem that cannot lock and answers `delete` to a request for
`wal`, is a refusal naming both — never a deployment running in a mode
its config does not say. Validated at startup against SQLite's own set.
That is Litestream's stanza, `sqlite3 -readonly` on a live node, and the
`busy_timeout` every ad-hoc query had to remember, all at once.
`connectors/sql/tests/scope.rs`,
`the_scopes_journal_mode_is_applied_on_open_and_verified`.

### §18 a credential reference — `landed`

`pass_env` on `ssmtp`, and `{"env": "NAME"}` as an injected `rest` header
value: `crypto`'s `key_env` mechanism, twice more. Read once at startup;
an unset variable is a refusal by name there; naming both `pass` and
`pass_env` is a refusal too. The rendered-from-`secrets.env` workaround
can become the config itself.

### §20 `Date`, and the `Message-ID` — `landed`, both halves

`Date` from the host clock in RFC 5322's shape, always `+0000`, without a
calendar crate (the days-to-civil arithmetic is pinned against three known
instants). `Message-ID` **generated and returned**, as the ask preferred
and for its reason: minted under the sender's domain as
`<secs.nanos.seq.rand@domain>`, written on the wire, and answered in
`message_id`, so a reply threads onto its own message and the duplicate
on the wire is self-diagnosing — one id twice is one send retried, two
ids is two sends. `scope.from` must name an `@domain` now, checked at
startup. `connectors/ssmtp/tests/send.rs`,
`a_message_carries_a_date_and_a_message_id_the_sender_is_told`.

### §22 regular expressions — `landed`, upstream

diluvium build13, pinned. A backtick literal, `regex.{find, match, gmatch,
gsub, split}` with the pattern first, a Thompson NFA with Pike's submatch
tracking — linear, no backtracking, backreferences and lookaround refused
by name with the reason. In the language, so in every profile and kept
by a sealed guest; measured here under `drt run` with build13's own
examples answering character for character, and `(a+)+b` against sixty
`a`s answering `nil` at once. `regex_extract` can be written.

### §23 proxy and roots — `landed`, the roots half

`extra_roots` on the `rest` scope: PEM files whose certificates are
trusted **beside** webpki's, never instead — the ask's own emphasis, kept
— parsed at startup with every certificate checked as a usable anchor, so
a wrong path or a key file handed over by mistake is refused by name
before the first call. `proxy` is open; see §4.

---

## 3. Landed before rc4, confirmed

- **§5 `tool` → `exec/run`** — the ask's own account is right, and its
  rule ("wrapper scripts only, never an interpreter") is now
  `doc/Python.md` §6, with the measurement that a venv interpreter and the
  system one are the same program to `allow`.
- **§17 threading headers** — v0.4.1.
- **§19 `close_notify`** — v0.5.0rc2, with the misdiagnosis the ask kept.
- **§21 the listener's window** — v0.5.0rc3, `admit_timeout_ms` covering
  the pre-declaration case; and §1 above for the rest of that section.
- **§3 `smtp` submission** — this is `ssmtp`, which landed after the ask
  was written and has the scope shape it describes: relay, fixed `from`,
  recipient allowlist, injected credential, STARTTLS that fails closed.
  The rate limit it wants is §2.

---

## 4. Open, sized

- **§1 per-instance connector scopes** — the largest, and the two code
  sites the ask names are the right two: `field_caps` takes strings and
  `dispatch` passes the wiring scope. What it takes: a spawn request
  carrying `{capability, scope}` entries, attenuation refusing a scope the
  parent's does not contain (which is per-connector: an origin list
  narrows by subset, a directory by prefix), and `route` intersecting the
  instance's scope with the wiring's before the connector sees either.
  Several days, and the intersection rule per connector is design work
  before code. It outranks everything else on this list, as the ask says.
- **§2 rate limiting** — open; the durability question the ask raises is
  the design decision, and it is the same one as the scheduler's.
- **§4 blob handles** — open; the pump's "cannot fit" versus "does not
  fit yet" distinction (`doc/Playwright.md` §4.3) is the first piece.
- **§6 `wall_ms` and `spawns` budgets** — open, pure DRT, about a day, as
  `doc/Next.md` sizes it. Not in rc4 only for time.
- **§7 unix-socket `rest`** — open, small.
- **§8 structured parsing**, **§9 vector search**, **§10 approval**,
  **§11 host-side metrics** — open; none started.
- **§12 snapshot identity** — open, and the ask's sizing (both halves
  small, a header-field rewrite) holds on reading `dsnap.c`.
- **§13 delivery between instances** — open; `Messaging.md` §7.3 as
  written, and the first consumer of SPEC §10's ref encoding.
- **§16 `sd_notify`** — open, small: `READY=1` once the root steps,
  `WATCHDOG=1` from the drive loop. The rc4 runtime change makes the
  signal it wants more meaningful, since "still stepping" and "a connector
  is busy" are now different facts.
- **§23 `proxy`** — open, about a day: CONNECT for https and absolute-URI
  for http on the `rest` scope. The roots half landed, and the sandbox this
  reply was written in is exactly the environment the ask describes, so
  the split between the halves was measured rather than argued. The live
  `rest` example there answers `error tls` as shipped; with the
  intercepting CA in `extra_roots` it answers **`ok 403`** — the TLS
  layer now completes against the proxy's certificate and reads its
  answer, and the 403 is the proxy refusing a request that did not come
  through CONNECT, which `curl --noproxy '*'` reproduces to the number.
  `extra_roots` is what makes the proxy's refusal legible; `proxy` is
  what makes it unnecessary. The second half is needed, not merely nice.
- **§24 the lifecycle split** — open; the argument that admitted bytecode
  is safer than supplied source is right, and it is the same registry
  work as `Program::Set`.
- **§25 SigV4** — open; option 1 (a host-side `crypto/sigv4`) is the one
  to build, and the ask's second problem — the secret would otherwise
  cross a shared `jobs` table — is the argument that decides it.

---

## 5. Corrections, kept in place

- **§21** assumed rc3 parks `rest`. It did not; rc4 does. See §1.
- **§19**'s note that "rc3 also inverts the drive loop" and that Vera's
  suite fails three modes on it stands, but the hypothesis attached to it
  does not, for the same reason.
- Nothing else in the asks was found wrong on reading. The three code
  citations checked (`swarm.rs:918`, `drt-connector/src/lib.rs:207`,
  `dsnap.c:363`) say what the asks say they say.
