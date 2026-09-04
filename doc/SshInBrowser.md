# SSH into a browser: what is measured, and what blocks it

**Status:** measurement, 2026-09-04, against russh 0.63.2 and DRT's `web`
profile. Every claim below was run, not reasoned about. **The upstream
half is now written and proven** (§4); DRT's half is not.

**The ask.** A standard `ssh` client, pointed through `drt tunnel` as a
`ProxyCommand`, reaching a terminal inside a page running DRT — the same
DRT anyone can import from a CDN, not a Lab-only capability.

---

## 1. The verdict

**Buildable, and the one blocker is now fixed.** A russh server and
client, both running in Chromium, complete a real SSH handshake — key
exchange, publickey authentication, a session channel, data echoed back
through the cipher — in **33 ms**. The patch is one commit on `v0.63.2`,
twelve files, +311/−38, and it changes nothing off wasm: `cargo test -p
russh --lib` is 175 passed before and after, with the same ten failures
both times (ssh-agent tests, no agent on the machine).

The estimate this replaces was "hard, probably a native helper". That was
wrong twice over: the hard part was never the crypto, and the fix was a
day rather than a port. What is left is **DRT's own**, and none of it
waits on anyone.

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

## 5. What is not decided

- **How long we carry the patch.** DRT points `[patch.crates-io]` at the
  fork, the way it already points at the ego crates, so nothing waits on
  review. What is open is only whether the PR lands and when.
- **Which profile pays the +456 KB.** `web` names its connectors
  explicitly, so this is a profile question with an existing answer shape.
- **`Send`.** `run_stream` requires `H: Send` and `R: Send`. In-memory
  that is free; a WebSocket-backed stream holds a `JsValue` and is not.
  The shape that asks upstream for nothing further is the one
  `XtermTerminal` already uses, and it is what DRT is building: the
  socket stays in JS, and the Rust stream holds only channel ends, which
  *are* `Send`. A separately spawned pump owns the socket and never
  enters russh's future.

## 6. The posture, since this is a listener in someone's browser

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
`exec`.
