# SSH into a browser: what is measured, and what blocks it

**Status:** built, 2026-09-04, against russh 0.63.2 and DRT's `web`
profile. Every claim below was run, not reasoned about. The upstream half
is written and merged into the fork (§4); DRT's transport and its SSH
server are built, and **a standard `ssh(1)` client reaches a DRT shell
inside Chromium in the browser suite** (§5).

**The ask.** A standard `ssh` client, pointed through `drt tunnel` as a
`ProxyCommand`, reaching a terminal inside a page running DRT — the same
DRT anyone can import from a CDN, not a Lab-only capability. Everything
but the `ProxyCommand` now works; the suite bridges the bytes over a TCP
port instead, which is the same job with a shorter wire.

---

## 1. The verdict

**Built.** A russh server and
client, both running in Chromium, complete a real SSH handshake — key
exchange, publickey authentication, a session channel, data echoed back
through the cipher — in **33 ms**. The patch is one commit on `v0.63.2`,
twelve files, +311/−38, and it changes nothing off wasm: `cargo test -p
russh --lib` is 175 passed before and after, with the same ten failures
both times (ssh-agent tests, no agent on the machine).

The estimate this replaces was "hard, probably a native helper". That was
wrong twice over: the hard part was never the crypto, and the fix was a
day rather than a port.

DRT's own half followed the same day: the transport, the server, and a
gate in which OpenSSH 9.6 authenticates with a publickey, gets a pty, and
runs `drt run hello.dlua` inside a page (§5). What is left is `drt
tunnel` in front of it and the wording in `GUARANTEES.md` (§7).

## 2. What the chain already has

| piece | state |
|---|---|
| `ssh -o ProxyCommand="drt tunnel wss://…"` | exists (README) |
| relay: park, claim, splice, presence, metering | exists, gated in CI |
| WebSocket in a page | exists: `ego_transport`'s `ws_browser.rs` |
| a terminal to put behind the session | exists: M8's `ego_cli::Terminal` |

That last row is the one worth noticing. `Terminal` is a trait, and M8
implemented it over xterm.js. An SSH channel is another implementation of
the same trait, so a session reaching a page reuses the editor, the shell
and the guest-completing Tab that already ship. It is the third consumer
of a seam that exists rather than a fourth thing to build.

## 3. What was measured

**russh compiles for the browser.** `--no-default-features --features
ring` swaps `aws-lc-rs` (C and assembly, no wasm path) for `ring`, and the
crate builds for `wasm32-unknown-unknown`. The client alone is **467,304
bytes** in release, against `drt_web_bg.wasm`'s current 1,893,140 — about
+25%, a profile question rather than a blocker.

**The executor is already solved, upstream, deliberately.**
`russh-util` ships wasm shims for the two things an async protocol needs:

```rust
#[cfg(target_arch = "wasm32")]
macro_rules! spawn_impl { ($fn:expr) => { wasm_bindgen_futures::spawn_local($fn) }; }
```

and an `Instant` backed by chrono. The server path calls that same
`russh_util::runtime::spawn`, so it inherits both. Whoever wrote this
intended russh to run on wasm; they scoped it to the client.

**The server is excluded by policy, not by accident.** It is not one
over-broad `cfg` — it is 23 sites across 10 files, including a *WASM-only
stub* `Config` in `negotiation.rs` written so the client can compile
without the server's. Un-gating it needs `msg.rs`'s server constants,
`cert.rs`'s certificate decode, and the stub reconciled with the real
type. All three were done here and the crate compiles.

**Two clocks then panic, and they are different problems.**

- `SystemTime::now()` in `server/encrypted.rs`, validating an SSH
  certificate's expiry. `wasm32-unknown-unknown` has no clock in std, so
  this panics with *"time not implemented on this platform"*. It is a
  security check that must not be skipped, and the fix is the shim
  `russh_util::time` already has for `Instant`, applied to the other
  clock. Done here, in ten lines.
- **`tokio::time::Instant`, which is the blocker.** It wraps
  `std::time::Instant`, so `now()` panics; and it has no constructor that
  does not take one, so the *type* is unusable on the target. The server
  computes auth-rejection deadlines with it on **every** authentication
  attempt, before anything is known to be rejected — so even a successful
  auth trips it. Nine uses in the server path, five in the client.

The client is usable on wasm despite those five because its defaults
leave `keepalive_interval` and `inactivity_timeout` unset, and
`future_or_pending(None, sleep)` never constructs a timer. That is
avoidance by configuration, not by design.

## 4. The upstream patch, written

The timer moved into `russh_util::time`, beside the clock already there:
a resettable `Sleep` with `sleep`/`sleep_until`/`timeout`, which is
`tokio::time`'s behind a newtype natively and `setTimeout` on wasm,
cancelled on drop so a reset cannot leave a stale timer to fire.
`Instant` gained `Add<Duration>` and an ordering, because that is what a
deadline needs, and `duration_since` saturates rather than panicking — a
page's clock can be stepped, and that should change a wait, not abort a
session.

Three things only compiling it revealed, none of them in the estimate
above:

- **`spawn`'s `Send` bound.** It is `tokio::spawn`'s requirement, not
  `spawn_local`'s, and on wasm it rules out every future holding a JS
  value — which is any timer built on `setTimeout`. Relaxed on that
  target only. This is the same bound §5 says a socket-backed stream
  needs relaxed, so the two move together.
- **`Elapsed` had to be owned** by `russh-util`: tokio's cannot be
  constructed outside tokio, and the wasm `timeout` has to return one.
- **`SystemTime::now()`** for certificate expiry needed a shim rather
  than a `cfg`. Skipping the check was never an option; it is the check
  that says a certificate has expired.

The rest is the exclusions themselves — `server`, the server-only message
constants, `PublicKeyOrCertificate::decode`, and the WASM-only stub
`Config` that stood in for the real one. The listener helpers stay
native, since `wasm32` has no listener to hand them; `run_stream` is the
transport-agnostic door and is available everywhere.

It reads as a contribution rather than a fork because it finishes a job
the crate visibly started: `russh-util` exists *because* someone wanted
russh on wasm and wrote `spawn_local` and a chrono `Instant` for it.

## 5. DRT's half, built

### The byte stream a page owns

`crates/drt-web/src/ws.rs`, exported as `DrtSocket`. The shape is the one
§4's `Send` note predicted, and it is the whole design: **the page owns
the socket and Rust owns two channel ends.** A `WebSocket` is a `JsValue`
and therefore not `Send`; `run_stream` wants `R: AsyncRead + AsyncWrite +
Send`. A stream holding the socket could not be handed to it, and a
stream holding channel ends can — the same split `XtermTerminal` already
uses for the keyboard, for the same reason.

Nothing in it imports a WebSocket API. A page hands over whatever it
has — a real `WebSocket`, an `RTCDataChannel`, a relayed pair, a test
double — and pumps three calls: `deliver` what arrived, `await
nextOutgoing()` for what Rust wants written, `close` when the wire goes.
The Rust side reads end of input when the page closes, and the page's
loop ends when Rust's consumer drops the stream, so neither half has to
be told twice.

What proves it: four tests natively — bytes both ways with a short read
left over, close as EOF rather than as an error, a write with nobody left
as `BrokenPipe`, and the `Send` bound itself — and `socket-echo` in the
browser suite, which runs the page's real loop against a Rust task that
upper-cases what it reads, so the answer cannot be an echo of the
delivery path. It comes back in two chunks and then EOF, in the
`release-small` module the release ships.

`startEcho` remains as the transport's own gate — a protocol's worth of
ambiguity removed — and ships deliberately: the browser gate builds the
shipping profile, so a diagnostic that is not in the artifact is a
diagnostic nothing tests, and a host wiring a socket up wants to check the
plumbing before a protocol is in the way.

### The server, and what a standard client gets

`crates/drt-web/src/ssh.rs`, exported as `DrtSshServer`. The posture is
the ssh *client* connector's pointed the other way, and it is in the types
rather than in a warning: no password method, and `Authorized` is a list
of `authorized_keys` lines with no "accept anyone" variant, so an empty
one authenticates nobody. That is the shape the ask named — make the
dangerous thing hard to reach by accident, not the capability hard to use.
A host key is the page's to keep; `generateHostKey` hands one back rather
than holding it, because a host key that changes on reload trains whoever
connects to click through the warning that says it changed.

The handler holds channel ends and nothing else, which is how it satisfies
`H: Handler + Send` — the same split as the stream below it, one layer up.
A second task, which is not `Send` and never enters russh, owns the JS
side. So §6's open question is answered rather than open.

What a client gets is the page's own shell. `ssh-terminal.js` turns a
session into the four things `attach` takes — `write`, `onData`, `cols`,
`rows` — and everything above that is M8's editor and `shell.js`,
unchanged. There is no second terminal implementation: `ego_cli`'s is the
one, over xterm.js in a tab and over a channel from a client.

Three tests natively (`crates/drt-web/tests/ssh.rs`): a named key gets a
shell that carries bytes both ways with the window it asked for, an
unnamed key is refused, and an empty authorized set refuses everybody.
Then `ssh-into-the-page` in the browser suite, which is the product's
claim run rather than argued: OpenSSH 9.6, its own key, `-tt` for a pty,
through a TCP bridge into the page, printing what the page's own runtime
printed.

Two things that gate found, both real and neither visible from the Rust
side alone:

- **`ssh` with a pipe for stdin requests a 0x0 pty.** Legal on the wire,
  and not a terminal: a line editor with no columns draws a prompt and has
  nowhere to put a keystroke. The server now hands such a client 80x24,
  which is what `sshd` does.
- **A window change had nowhere to land.** The size is now an atomic the
  handler updates and the shell reads, and `ego_cli` asks a terminal its
  size on every keystroke — so a client resizing its window is picked up
  with no event plumbing on the page's side at all.

## 6. What is not decided

- **How long we carry the patch.** DRT points `[patch.crates-io]` at the
  fork, the way it already points at the ego crates, so nothing waits on
  review. What is open is only whether the PR lands and when.
- **Whether `web` keeps paying for it.** Measured rather than estimated,
  and the estimate was low: `drt_web_bg.wasm` goes from **1,915,072 to
  3,436,485 bytes**, +79%, or 1,089,348 gzipped. The +456 KB in §3 was the
  client alone in a crate of its own; a server, both key exchanges and the
  cipher suite cost more than that.

  It ships in `web` for now, and the reason is the gate rather than the
  bytes: a second artifact means either running the browser suite twice or
  shipping the untested one, and a release-only failure reaching a
  rehearsal is exactly what building `release-small` in CI was for. The
  `web` connector list is unchanged (`time`, `fs`, `crypto`) — this is a
  server, not a connector — so nothing a package declares resolves
  differently. Revisit if a page that wants no server has to care.
- **`drt tunnel` in front of it.** The suite bridges TCP; the product
  bridges a relay and a WebSocket. Same bytes, and `drt tunnel` already
  exists — what is untried is the two of them end to end.

## 7. The posture, since this is a listener in someone's browser

Named here because the capability is the point and hiding it would be the
wrong lesson. What DRT already has is the answer: whoever reaches the
session drives a **sealed guest holding whatever grants the page gave
it**, so the blast radius is the capability set and not the browser. On
top of that, and matching the ssh *client* connector's existing posture —
pubkey only, no passwords, authorized keys named explicitly, never
trust-on-first-use — plus the relay's own park and caller keys, which
gate reachability before SSH begins.

The accident worth engineering against is narrower than "a user does
something dangerous": **a page enabling the server without realising it
is reachable from outside the tab.** That is a defaults-and-wording
problem, and `GUARANTEES.md` should say it as plainly as it says it for
`exec`. What is in the code already: no password method, no way to
authorize a key without naming it, and an empty list that admits nobody
rather than everybody. What is not yet written is the sentence in
`GUARANTEES.md`.
