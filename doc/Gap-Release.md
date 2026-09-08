# The gap release — v0.5.0rc7g1

**Written 2026-09-08, against `780071d`** (v0.5.0rc7 plus the `wss://` merge,
PR #19). Every size below was taken by reading the code and names the file and
line it rests on, per `doc/Next.md`'s rule. Two claims made in the source
documents are corrected in place rather than quietly fixed.

A **gap release** is a self-contained cut of DRT made *during* the
architecture sprint, carrying work that is helpful but not critical, so that
it reaches consumers without either side waiting for the other.

---

## surface block

1. The selection test, and why it is not the test `vera:doc/DRT_GAP_RELEASE.md`
   applied.
2. In: the items, sized.
3. Out: what fails the test, and which test it fails.
4. Contested: the two asks where the tests disagree. Decisions needed.
5. The version string is a blocker. `script/changelog.py:318` refuses
   `0.5.0rc7g1`.
6. Sequencing.

---

## 1. The selection test

`vera:doc/DRT_GAP_RELEASE.md` chose its two asks by one test: *can an HTTP
sidecar substitute for this during the freeze?* That test selects for things
that are **critical** — it is a list of what a freeze makes worse.

This release is scoped by the opposite test, and the difference is not
cosmetic. An item belongs here when:

1. **It is self-contained in DRT.** No `dv_abi` bump, no diluvium pin move, no
   cross-repo coordination — which `doc/Next.md` §1 already names as "usually
   the expensive part".
2. **It is additive.** A consumer sitting on rc7 who does nothing keeps
   working, byte for byte.
3. **It requires no adoption.** The consumer gets the benefit by upgrading,
   not by writing a scope, changing a spawn call, or editing a config.
4. **It is off the sprint's path.** The sprint will not have to re-land it,
   and it does not commit us to an interface the sprint may obsolete.

Criterion 3 is the one that does the work, and it is what "run in parallel to
their intended consumers" actually means. An item a consumer must *integrate*
is not parallel — it is a second project, scheduled inside a freeze, in the
repository least able to absorb it.

The consequence is that this release is mostly **diagnostics, RFC
conformance, and documentation**. That is not a consolation prize. The single
highest-support-cost item on either list (issue #21) is a diagnostics bug, and
it very nearly cost a working design.

---

## 2. In

### 2.1 Interface errors that match their errno — issue #21, first half

`crates/drt/src/wireguard.rs:456` attaches the CAP_NET_ADMIN paragraph to
every error `create_as_async` can return. ENOENT (no `/dev/net/tun`, in a
container) and EACCES (that node at mode 0600) both arrive dressed as a
missing privilege — to a process that is already uid 0, or that holds
`cap_net_admin=ep`. EPERM is the only one of the three the sentence is true
for.

**The unknown in the issue is resolved, and the answer is the good one.**
Issue #21 hedges: *"If distinguishing the kinds through gotatun's error type
is awkward"* — and offers a weaker fallback wording. It is not awkward.
`gotatun::tun::tun` re-exports the `tun` crate, pinned at `0.8.14`
(`Cargo.lock:4321`); `create_as_async(&Configuration) -> Result<AsyncDevice,
tun::Error>`; `tun::Error` carries an `Io(std::io::Error)` variant and is not
`#[non_exhaustive]`. So `match e { tun::Error::Io(io) => io.kind(), _ => .. }`
reaches the errno, and the issue's suggested fix is implementable **as
written**. The fallback wording is not needed and should not be taken.

One closure, at one call site. No signature changes anywhere.

**~2 hours**, plus tests. The tests are the interesting half: `NotFound` and
`PermissionDenied` need constructing without a privileged environment, so the
branch wants lifting into a `fn hint(kind: Option<ErrorKind>) -> &'static str`
that a unit test drives directly — which is also what keeps the three
sentences in one readable place rather than inside a `format!`.

### 2.2 `drt wg check` looks at the environment — issue #21, second half

`Command::Wg { action: Some(WgAction::Check) }` (`crates/drt/src/cli.rs:822`)
calls `wireguard::validate` (`wireguard.rs:179`), which reads the config and
nothing else. It printed `ok: drt0 on port 51821` on both machines where the
tunnel then could not come up. `check` is the verb whose whole job is *will
this work here*.

**A correction that changes the shape of the fix.** The obvious move — put the
probe in `validate` — is wrong, and the reason is written in `validate`'s own
code: `bind` validates too (`wireguard.rs:233-234`, "Returned rather than
printed: `bind` validates too"). Environment probing inside `validate` would
run on every `drt start`, where a false negative is a refusal to boot rather
than a note. The probe belongs in a separate function called **only** from the
`wg check` arm.

**And it must not repeat issue #21's own disease.** `/dev/net/tun` is a Linux
fact. macOS has no such node (utun is allocated through a `PF_SYSTEM` socket)
and Windows needs `wintun.dll` beside the binary. A preflight that reports a
missing `/dev/net/tun` on macOS would be asserting a cause the platform
contradicts — the exact bug this item exists to fix. So: three `cfg` arms, and
the honest answer on a platform we have not implemented a probe for is
silence, not a guess.

Two facts on Linux, both cheap and both decisive: the node exists, and this
process can `OpenOptions::new().read(true).write(true).open()` it. Reported as
warnings on the existing `Vec<String>` channel, so exit codes do not move —
`wg check`'s "a warning is not a failure" rule (`cli.rs:834-836`) already
covers it.

**~½ day** including the platform split and tests.

### 2.3 `netcheck --ca-file`

`netcheck` takes no scope, so it still fails `UnknownIssuer` behind an
intercepting proxy — and netcheck is what we tell people to run when they do
not know their own network. A corporate network is exactly where that question
is hardest to answer.

**Verified, and the cheap route is real.** `doc/Ask-Discofetch-Reply.md:85-89`
predicted this would land "the day `netcheck` takes a scope for it". It does
not need one. The plumbing:

- `netcheck`'s HTTPS goes through `crates/drt/src/reflect.rs`, which builds its
  own root store inline at `reflect.rs:139-140` — webpki roots only.
  `reflect.rs:15` says the two share tokio-rustls and webpki-roots as *crates*,
  and that is exactly right: they share no code, so there is nothing to
  refactor around.
- `rest` already has the parser: `load_roots`
  (`connectors/rest/src/lib.rs:315`) reads PEM files, refuses an empty file, a
  wrong path, or a key file handed over by mistake, and probes that webpki can
  actually use each certificate as a trust anchor. That function is ~25 lines
  and depends on nothing in `rest`. Lift it to a shared home, or duplicate it —
  duplicating is defensible here and worth deciding rather than defaulting.
- Thread `&[CertificateDer]` through `reflect::get` (`reflect.rs:48`) to
  `fetch` (`reflect.rs:121`). Two call sites in netcheck, `netcheck.rs:841` and
  `netcheck.rs:1017`. `addresses`/`addresses_at` do no TLS and are untouched.
- One clap arg beside the others at `cli.rs:239`.

Added beside webpki's roots, never instead — the rule `rest`'s scope doc
already states (`connectors/rest/src/lib.rs:215-222`), and for the same
reason: a flag that could narrow the trust store to one certificate is a
footgun.

**~½ day.**

### 2.4 `drt turn` answers 400 where the RFC says 401 — issue #17, nits

RFC 5389/8489 says a wrong or expired credential gets 401 with a fresh NONCE.
`drt turn` answers 400, which is **terminal** to a browser's ICE agent — it
does not retry a 400. Verified in issue #17 with a hand-built Allocate on rc4.

This is a conformance bug against a spec, not a feature: no consumer adopts
anything, and the current behaviour is simply broken for the client it exists
to serve. Textbook gap-release item.

**Not sized here.** I did not read `turn`'s error path, and sizing it from the
issue's description would break this document's own rule. Triage first.

### 2.5 Three documentation sentences, all asked for by name

Free, and each one is an afternoon somebody already lost:

- **`doc/WireGuard.md` §2 step 4** — a Linux NAT with an open INPUT chain
  confirms the conntrack entry for the far peer's first unsolicited probe and
  then remaps the outbound flow off the STUN-measured port (51820 → 65038 in
  discofetch's lab). Explains a punch that "succeeds" and then never
  handshakes. (Issue #17, nits.)
- **Beside `last_handshake_ms`** — *"an age is a fact about the moment the
  snapshot was sent; do not assume one keeps arriving."* With the peer gone the
  device eventually stops reporting an age at all, so a health check that reads
  only the field goes quiet exactly when the link is deadest. discofetch hit
  this and asked for the sentence explicitly (issue #17, comment 3).
- **Beside `relay`** — which of the two things happens today when a credential
  expires under a live allocation. Issue #17 §3 says a note "would already
  help", and §3 itself is not in this release.

### 2.6 To triage, not yet sized

Both are self-contained bug fixes that would pass the test if they size small.
I have not read either code path, so they are named as candidates and nothing
more:

- **#11** — `listen` answers 503 immediately when the target queue does not
  exist yet, ignoring `admit_timeout_ms`.
- **#10** — every HTTPS response AWS closes without `close_notify` fails as
  `unexpected_eof`.

---

## 3. Out

### 3.1 `--tun-fd` / an inherited descriptor — fails criterion 4

A privileged helper makes the device, DRT runs with no capability at all.
Better than setcap, which we currently have to explain to every customer at
their terminal.

It is out for one reason: **issue #17 §7 may obsolete it entirely.** §7's
no-root mode terminates TCP in-process over a userspace stack (smoltcp),
touching only the IP side — which is already a trait. That removes the device
rather than moving who creates it. Shipping `--tun-fd` in a gap release commits
us to an fd-inheritance contract, socket-activation semantics, and a helper
we then support, possibly for one release. That is precisely the decision the
sprint should own.

What this release does about the pain in the meantime is §2.1 and §2.2: the
setcap conversation gets much shorter when the error stops sending people to
grant a privilege they already hold.

### 3.2 Windows and macOS artifacts — issue #17 §4, §5

The Windows profile with `wireguard` + `netcheck` + `wintun.dll`, and signed
darwin binaries. These fail criterion 3 hard — they are release-machinery and
signing-identity work, not a code delta — and they are also *genuinely
blocking* discofetch ("what stands between this and a laptop that is not
Linux", issue #17 comment 1). Blocking work belongs on the critical path, not
on the gap list. Naming them here so nobody reads their absence as a
deprioritisation.

### 3.3 Vera Ask A — per-instance connector scopes — fails 1, 3 and 4

`Grant` already carries an optional `scope` (`drt-caps/src/lib.rs:77`), and
vera's doc is right that two independent things stop it working. Both confirm
on read:

- `field_caps` (`drt-swarm/src/swarm.rs:1211-1240`) takes a spawn request's
  caps as strings — `swarm.rs:1230` is `let rmpv::Value::String(s) = entry else
  { return Err(()) }`. A spawn cannot *express* a narrowed scope.
- Dispatch passes the process's wiring scope: `scope: wired.scope.clone()`
  (`drt-connector/src/lib.rs:216`).

**One piece of good news for whoever does size this.** `Dispatcher::route`
(`drt-connector/src/lib.rs:180`) already holds the `CapSet` — it calls
`caps.holds(...)` at `:190` — so the instance's own grant is *in hand* at the
exact line that discards it. The information is not missing; only the
intersection is.

**And that is the whole problem.** "Intersect the instance's grant scope with
the wiring scope" is per-capability semantics: origin lists for `rest`,
subtrees for `fs`, tables for `sql`. `ScopeType` today only *validates*
(`drt-connector/src/lib.rs:44`, `:109`). Intersection is a new operation on
that trait, and it changes the spawn wire format — a caps entry that is not a
string. A wire-format change and a new capability-model operation, landed
inside a freeze, in a release whose purpose is to be safe to take.

Vera's argument that this gets *worse* to live without during the freeze is
sound and is not disputed here. It is an argument for scheduling it — not for
putting it in this release.

---

## 4. Contested — decisions needed

### 4.1 Vera Ask B — unix-socket transport for `rest`

This is the one where the two tests genuinely disagree. Vera puts it on the
critical list. It passes criteria 1, 2 and 4 cleanly and fails only criterion
3, and it fails that one softly: a consumer must write a scope to benefit, but
existing scopes are untouched.

**It is more tractable than its neighbours, and one fact decides that.**
`exchange` (`connectors/rest/src/lib.rs:949`) is already generic —
`async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin>` — so a
`UnixStream` drops straight in with no change to the I/O half at all.
`request_bytes` is transport-agnostic too. What is left is the connect path:
`lookup_host` at `:895`, the `permits_address` check at `:905`, and
`TcpStream::connect` at `:913` all assume a socket address, and a unix path
must branch before all three.

The open question is not mechanical, it is semantic: **what is an "origin" for
a unix socket?** `AllowEntry.origin` is a parsed `Url`
(`connectors/rest/src/lib.rs:233`), and the allowlist matches on it. A path
needs a new entry shape and an answer for what `Host` the request carries.
That is a scope-model decision, and it is small only if we make it quickly.

**~2 days once that question is answered. Your call whether it rides here.**

### 4.2 The version string is a blocker

`0.5.0rc7g1` **does not validate today**:

```python
# script/changelog.py:318
m = re.fullmatch(r"(\d+\.\d+\.\d+)(?:rc\d+)?", str(version))
```

This is not cosmetic. `changelog.py release-check --tag` runs in the release
preflight (`.github/workflows/release.yml:97`) on every tagged build, and
`changelog.py check` in CI is what stops the committed `CHANGELOG.md` and
`changelog.json` going stale. A tag the validator rejects cannot be published
by the existing pipeline.

Widening the regex is one line. The comment above it is not — `changelog.py:311-317`
deliberately documents the naming rule and why crate versions stay at `X.Y.Z`,
and it would have to say what a `gN` suffix means. That comment is load-bearing.

Two other consequences, both benign and both worth knowing:

- **Ordering is safe.** The newest-entry check reads `doc["releases"][0]` —
  list order, not a version sort — and `latest/` resolves through the explicit
  `latest: true` flag, not a comparison. Nothing in our machinery sorts version
  strings. Anything downstream that *does* will order `rc7g1` wrong.
- **No consumer's `requires` breaks.** The comparisons run against the base
  `X.Y.Z` (`changelog.py:322`), which stays `0.5.0`.

**Recommendation: cut it as `v0.5.0rc8`, and let the changelog carry the
"nothing moved underneath you" claim.** That claim is exactly what a `g`
suffix is trying to signal, and `CHANGELOG.yaml` already records it *as
checkable fact* rather than as a naming convention: `dv_abi`, the embedded
`diluvium` revision, and the per-profile `connectors` set. If those three are
identical to rc7's — and on the scope above they are — then the release is
provably a superset in the only way that matters to a consumer, and
`BUILDINFO.txt` says so on the artifact. That is the repository's own stated
philosophy: *"the compatibility fact travels with the bytes"*, and *"records
the coupling instead of encoding it"* (`doc/Release.md`). A `g1` suffix encodes
in a tag name what the changelog already records honestly, and costs the one
regex that keeps version strings from drifting.

**If you want `rc7g1` anyway, it is cheap** — one regex, one rewritten
comment, and an entry in `doc/Release.md` defining the suffix so the next
person does not have to reverse-engineer it. Say the word and it ships that
way; the name is yours to pick, and there is a real argument that a tag which
*looks* like a sibling of rc7 communicates faster than a changelog field
somebody has to open.

---

## 5. Sequencing

Nothing here blocks anything else here, which is the point.

| | item | size | risk |
|---|---|---|---|
| 1 | §2.1 errno hints | ~2 h | none; unknown resolved |
| 2 | §2.5 three doc sentences | ~1 h | none |
| 3 | §2.2 `wg check` preflight | ~½ day | platform split |
| 4 | §2.3 `netcheck --ca-file` | ~½ day | none |
| 5 | §2.4 TURN 401 | triage | unsized |
| 6 | §2.6 #10, #11 | triage | unsized |

**About a day and a half of established work**, plus whatever triage adds.

Two things to do before cutting, neither of them code:

- **Settle the version string** (§4.2). It gates the tag, not the work.
- **Re-run the examples.** This repository's own traps list says to when
  config parsing changes. Nothing here changes config parsing — §2.2 adds
  warnings to an existing channel and §2.3 adds a CLI flag — so this should be
  a confirmation rather than a fix. `examples/` is a declared human surface
  (`.claude/rules/human-surfaces.md`), and `examples/21-wireguard/` and
  `examples/22-wireguard-interface/` are the two that touch §2.1 and §2.2.

## 6. Decisions I need before starting

1. **The version string.** `rc8` (recommended) or `rc7g1` (one regex, one
   comment, one line in `doc/Release.md`).
2. **Does Ask B ride here** (§4.1), and if so, what is an origin for a unix
   socket?
3. **Triage #10, #11 and the TURN 401 into the release, or leave them out?**
   I can size all three before you decide; none is sized now, and I would
   rather say so than guess.
4. **`load_roots`: lift or duplicate?** (§2.3.) Lifting couples `netcheck` to
   a crate it does not otherwise need; duplicating ~25 lines keeps `reflect`'s
   deliberate independence from `rest`. I lean duplicate, and it is a
   two-minute decision that is annoying to reverse later.
