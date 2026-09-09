# The gap release — v0.5.0rc8

**Written 2026-09-08, against `eba11c7`,** and revised the same day against
what had already landed. Sizes name the file and line they rest on, per
`doc/Next.md`'s rule.

A **gap release** is a self-contained cut of DRT made *during* the
architecture sprint, carrying work that is helpful but not critical, so that
it reaches consumers without either side waiting for the other.

---

## surface block

1. The selection test, and why it is not the test
   `vera:doc/DRT_GAP_RELEASE.md` applied.
2. What is in, and what it cost.
3. What is out, and which test it failed.
4. The two vera asks, deferred, with the argument for scheduling them.
5. Settled: the version, and one thing not to re-litigate.

---

## 1. The selection test

`vera:doc/DRT_GAP_RELEASE.md` chose its two asks by one test: *can an HTTP
sidecar substitute for this during the freeze?* That test selects for things
that are **critical** — it is a list of what a freeze makes worse.

This release is scoped by the opposite test. An item belongs here when:

1. **It is self-contained in DRT.** No `dv_abi` bump, no diluvium pin move,
   no cross-repo coordination — which `doc/Next.md` §1 already names as
   "usually the expensive part".
2. **It is additive.** A consumer sitting on rc7 who does nothing keeps
   working, byte for byte.
3. **It requires no adoption.** The consumer gets the benefit by upgrading,
   not by writing a scope, changing a spawn call, or editing a config.
4. **It is off the sprint's path.** The sprint will not have to re-land it,
   and it does not commit us to an interface the sprint may obsolete.

Criterion 3 is the one that does the work, and it is what "runs in parallel
to its consumers" actually means. An item a consumer must *integrate* is not
parallel — it is a second project, scheduled inside a freeze, in the
repository least able to absorb it.

The consequence is that this release is diagnostics and documentation. That
is not a consolation prize: the highest-support-cost item on either list
(issue #21) was a diagnostics bug, and it nearly cost a working design.

---

## 2. In

### 2.1 Interface errors that match their errno — issue #21 — **landed**

Landed in PR #22 (`85958ff`), both halves, before this document's first
draft was finished. Recorded here because the sizing it was given is worth
keeping honest, and because one thing that draft said is now checkable.

Issue #21 hedged that reaching the errno through gotatun's error type might
be awkward, and offered a weaker fallback wording. It is not awkward, and
the fallback was correctly not taken: `gotatun::tun::tun` re-exports the
`tun` crate (`Cargo.lock:4321`, 0.8.14), `create_as_async` returns
`Result<AsyncDevice, tun::Error>`, and `tun::Error` carries
`Io(std::io::Error)` and is not `#[non_exhaustive]`.

What shipped is better than the issue asked for: a lookup table
(`wireguard.rs`, `INTERFACE_ERRNO`) with four errnos, **per platform** —
macOS allocates a `utun` through a kernel control socket with no device
node, so an `ENOENT` there is answered with silence rather than with Linux's
answer. An errno outside the table gets no sentence rather than a guess.

`drt wg check` grew `here:` lines for the two facts it can establish without
creating anything, and the exit code still follows the config alone, so a
config written on a laptop and deployed where the privilege is does not fail
on the laptop.

### 2.2 `netcheck --extra-root <PEM>` — **landed here**

The open half of the ask. `netcheck` is what a person is told to run when
they do not know their own network, and behind an intercepting proxy it
answered `address    not measured` — honest and useless.

`doc/Ask-Discofetch-Reply.md:85-89` expected this to wait for `netcheck` to
take a scope. It did not need one: `reflect` builds its own trust store, so
a flag was enough — which is what the ask itself predicted ("it needs none
of the scope machinery").

Named `--extra-root` and not `--ca-file`, because PR #22's sibling landed
`drt tunnel --extra-root <PEM>` the same day. Two flags doing one job under
two names, on two verbs of one binary, is a wart that gets harder to remove
later.

**One loader for both.** `tunnel::load_roots` moved to `crates/drt/src/roots.rs`,
gated on `any(feature = "tunnel", feature = "netcheck")` — both features
already pull `tokio-rustls` and `webpki-roots`
(`crates/drt/Cargo.toml:174`, `:220`), so the gate costs nothing. `roots::store`
is now the single place that decides what a DRT client trusts, so *added,
never substituted* is a property of one function rather than of every caller
remembering it. Moved rather than duplicated **while it is still
unreleased**, which is the only cheap moment to move a `pub` item.

The shared loader also took `rest`'s stricter check, which the tunnel's copy
did not have (`connectors/rest/src/lib.rs:315`): a certificate that parses
but cannot anchor is refused at load, by name, rather than accepted there
and rejected at dial. That is the promise the flag's own first sentence
makes.

**Correction to this document's first draft**, which asked whether to lift
or duplicate `rest`'s loader. Neither: `rest` is a separate crate and its
refusals name a config key where these name a flag, so it cannot and should
not share. The sharing that mattered was `tunnel` ↔ `netcheck`, which the
draft missed because `tunnel::load_roots` did not exist when it was written.

### 2.3 Documentation — **landed**

- **The conntrack finding** (issue #17, nits) — landed in PR #22,
  `doc/WireGuard.md` §2 step 4, and framed as an argument *for* re-askable
  mapping rather than as a caveat.
- **`last_handshake_ms` is an age** — landed here. A program that reads
  staleness off the field goes quiet exactly when the link is deadest,
  because the device eventually stops reporting an age at all. discofetch
  lost an afternoon to it and asked for the sentence by name (issue #17,
  comment 3). The rekey-interval floor is beside it, since their 20 s
  threshold flagged every healthy link.
- **The relay credential's expiry** (issue #17 §3) — already documented at
  `doc/WireGuard.md:414`, with what follows from the design and what has
  not been measured kept apart.

---

## 3. Out

### 3.1 The TURN 401 — **not ours**

Listed as a candidate in this document's first draft, wrongly. RFC 8489
§9.2.4 says a wrong or expired credential gets 401 with a fresh NONCE and
the `turn` crate answers 400, but that is upstream:
`doc/TURN-401-Upstream.md` is the brief, and
`a_forged_credential_is_refused_with_the_wrong_code_for_now` is the tripwire
that goes red when upstream fixes it. Nothing to do here.

### 3.2 `--tun-fd` / an inherited descriptor — fails criterion 4

A privileged helper makes the device and DRT runs with no capability at all
— better than setcap, which we explain to every customer at their terminal.

Out because **issue #17 §7 may obsolete it entirely**: the no-root mode
terminates TCP in-process over a userspace stack, touching only the IP side,
which is already a trait. That removes the device rather than moving who
creates it. Shipping `--tun-fd` here commits us to an fd-inheritance
contract and a helper we then support, possibly for one release. The sprint
should own that.

§2.1 is what this release does about the pain meanwhile: the setcap
conversation is much shorter once the error stops sending people to grant a
privilege they already hold.

### 3.3 Windows and macOS artifacts — issue #17 §4, §5

They fail criterion 3 hard — release-machinery and signing-identity work,
not a code delta — and they are *genuinely blocking* discofetch ("what
stands between this and a laptop that is not Linux"). Blocking work belongs
on the critical path, not the gap list. Named here so their absence is not
read as a deprioritisation.

### 3.4 #10 and #11 — **both already fixed; the issues are stale**

Triaged rather than sized, and the sizing turned out to be the wrong
question: both shipped in **v0.5.0rc3**, both with tests, and both issues
are still open. Nothing to do here except close them.

**#10** — `rest` read to the connection's end rather than to the body's
framing, so a server that closes without a TLS `close_notify` (Bedrock, and
plenty besides) failed a response whose every byte had arrived. Fixed by
`e8daa8c`, "`rest` reads to the body's framing, not to the connection's
end": the loop now stops where `content-length` or the terminating chunk
says the body ends, so **there is no such read** — the error is not
swallowed, it is never raised. A body genuinely cut short still fails, and
`connectors/rest/src/lib.rs:1068` (`short_body`) names what was expected and
what arrived, which is the message improvement the issue asked for in its
last paragraph. Four tests, including a `close`-framed body (where the close
*is* the framing) and a truncation that must still fail.

**#11** — landed as the issue's own first-preference fix: `admit_timeout_ms`
now applies to this case too, so a request naming a queue the program has
not declared yet waits for it. The wait is `start::retry_held`
(`crates/drt/src/start.rs:658`) and not the acceptor's, because whether a
queue exists is a question only the deployment can answer; `listen.rs`'s
module doc carries the reasoning. Three tests, and the third is the one
worth noting: `a_grace_of_zero_refuses_before_the_program_can_declare` keeps
the old behaviour reachable as a setting rather than deleting it, since
`admit_timeout_ms = 0` is exactly what the bug was.

**#15** — swept at the same time and the same story: all three of its
fixes shipped in **v0.5.0rc6**, with tests. The held one-shot reports
landed close to the submitted patch but **bounded** (`HELD_MAX = 64`,
`crates/drt/src/wireguard.rs:139`), dropping the newest past the cap rather
than the oldest, because the report a program blocks on is the mapping and
the mapping is sent first. `peers = {}` was fixed past what it asked for —
one `list()` helper (`crates/drt/src/config.rs:145`) that resolves Lua's
`{}` ambiguity for every list read, so `caps`, `headers`, `stun` and
`allowed_ips` closed with it.

**Worth a note for whoever files next.** All four issues swept here — #10,
#11, #15 and #21 — were fixed within a day or two of being filed, and none
was closed by the fix. Three of them sat on this list as open work through
two or more candidates, and #15 had been confirmed fixed by its own
reporter, in a comment on another issue, the same day it landed. The
changelog was right about all four; the tracker was right about none. It is
the least reliable source in this repository, and it is the one a release
gets scoped against.

---

## 4. The two vera asks — deferred, and worth scheduling

Both are deferred from this release. Neither is refused, and the reason for
deferring is criterion 3 in both cases, not size.

### 4.1 Per-instance connector scopes (`DRT_ASKS` §1)

Michael's argument for this is stronger than the freeze argument vera's
document makes, and is recorded here because it changes what the work is
for: **granular scoping is what makes it safe to let autonomous agents write
code that runs inside DRT.** That is not a freeze-shaped concern that passes
when the freeze ends; it is a property the capability model either has or
does not, and everything built on the coarse version has to be revisited
when it arrives. It gets more expensive to add, not less.

What stands in the way, both confirmed by reading:

- `field_caps` (`drt-swarm/src/swarm.rs:1211-1240`) takes a spawn request's
  caps as strings — `swarm.rs:1230` is
  `let rmpv::Value::String(s) = entry else { return Err(()) }`. A spawn
  cannot *express* a narrowed scope.
- Dispatch passes the process's wiring scope:
  `scope: wired.scope.clone()` (`drt-connector/src/lib.rs:216`).

**One piece of good news for whoever sizes it.** `Dispatcher::route`
(`drt-connector/src/lib.rs:180`) already holds the `CapSet` — it calls
`caps.holds(...)` at `:190` — so the instance's own grant is in hand at the
exact line that discards it. The information is not missing; only the
intersection is.

And that is the real cost. "Intersect the instance's grant scope with the
wiring scope" is per-capability semantics: origin lists for `rest`, subtrees
for `fs`, tables for `sql`. `ScopeType` today only *validates*
(`drt-connector/src/lib.rs:44`, `:109`). Intersection is a new operation on
that trait, and expressing a scope in a spawn changes the spawn wire format.
A wire-format change plus a new capability-model operation is not a gap
release; it is a piece of the architecture, and it should be scheduled as
one.

### 4.2 Unix-socket transport for `rest` (`DRT_ASKS` §7)

Nice to have, and cheaper than it looks — the intuition that the machinery
is already there is right in the half that usually costs most. `exchange`
(`connectors/rest/src/lib.rs:949`) is already generic over
`S: AsyncReadExt + AsyncWriteExt + Unpin`, so a `UnixStream` drops in with no
change to the I/O half, and `request_bytes` is transport-agnostic too.

What is left is the connect path — `lookup_host` (`:895`), the
`permits_address` check (`:905`) and `TcpStream::connect` (`:913`) all
assume a socket address, and a unix path must branch before all three — and
one question that is semantic rather than mechanical: **what is an "origin"
for a unix socket?** `AllowEntry.origin` is a parsed `Url`
(`connectors/rest/src/lib.rs:233`) and the allowlist matches on it, so a path
needs a new entry shape and an answer for what `Host` the request carries.

~2 days once that question is answered. It is small only if the answer is
decided first, which is why it is not being started inside a release whose
point is to be safe to take.

---

## 5. Settled

**The version is `v0.5.0rc8`**, and the entry was already open in
`CHANGELOG.yaml` when this document proposed it.

**One thing not to re-litigate: the version scheme.** A candidate is
`X.Y.ZrcN`, crate versions stay at `X.Y.Z`, and `script/changelog.py:318`
enforces exactly that. A suffixed variant (`rc7g1`, to signal "rc7 plus a
safe delta") was considered and dropped: the claim it encodes in a tag name
is one `CHANGELOG.yaml` already records as *checkable fact* — `dv_abi`, the
embedded `diluvium` revision, and the per-profile `connectors` set, all
identical to rc7's here, and all carried on the artifact by `BUILDINFO.txt`.
That is `doc/Release.md`'s stated philosophy: records the coupling instead
of encoding it, and the compatibility fact travels with the bytes. Changing
the scheme would cost the one regex that keeps version strings from
drifting, to say something the file already says better.

## 6. Verification

333 tests, `clippy --workspace --all-features --all-targets` clean, and
`cargo fmt --check` clean, on Rust 1.95 — which is the floor `gotatun 0.9.2`
sets and therefore the floor for any build carrying `wireguard`.

The `--extra-root` work is covered at both altitudes, deliberately.
`crates/drt/tests/reflect.rs` stands a TLS terminator in front of the test
edge and asserts **both halves in one run** — the same edge unreachable
without the flag and measured with it — because only the pair proves the
flag did the work. `roots.rs`'s own tests assert the property an integration
test structurally cannot see: that naming a CA *adds* to the public roots
rather than replacing them. A client that quietly narrowed its trust to the
one named CA would pass every end-to-end test above and then refuse every
ordinary host in the field, so that one is a count, and the count is a unit
test.

Feature combinations built and checked, since `roots` is gated on either of
two: `slim`, `slim,netcheck`, `slim,tunnel`, `full`, and `wasi`.
