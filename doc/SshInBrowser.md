# SSH into a browser: what is measured, and what blocks it

**Status:** measurement, 2026-09-04, against russh 0.63.2 and DRT's `web`
profile. Every claim below was run, not reasoned about. Nothing is built.

**The ask.** A standard `ssh` client, pointed through `drt tunnel` as a
`ProxyCommand`, reaching a terminal inside a page running DRT — the same
DRT anyone can import from a CDN, not a Lab-only capability.

---

## 1. The verdict

**Buildable, and blocked on one thing that is upstream's.** The crypto,
the protocol and the executor all work on `wasm32-unknown-unknown` today.
What does not is `tokio::time`, which the SSH *server* path needs in nine
places and which cannot work on that target at all.

The estimate this replaces was "hard, probably a native helper". That was
wrong in both directions: the hard part is not the crypto, and the fix is
smaller than a helper — but it is not ours to make.

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

## 4. What the upstream ask is

Route the server's deadlines through `russh_util::time` rather than
`tokio::time`, and add a `sleep` there over `setTimeout` — beside the
`spawn` and `Instant` shims that module already exists to hold. Then
compute the auth-rejection deadlines lazily, where a rejection is
actually being sent, rather than at the top of the handler.

That is a coherent contribution rather than a fork: it finishes a job the
crate visibly started, and the two shims it extends are already theirs.

## 5. What is not decided

- **Whether we wait or carry a patch.** A vendored russh is a real cost;
  so is blocking on an upstream that has no reason to hurry.
- **Which profile pays the +456 KB.** `web` names its connectors
  explicitly, so this is a profile question with an existing answer shape.
- **`Send`.** `run_stream` requires `H: Send` and `R: Send`. In-memory
  that is free; a WebSocket-backed stream holds a `JsValue` and is not
  `Send`. The shape that avoids asking upstream for anything more is the
  one `XtermTerminal` already uses: the socket stays on the JS side and
  the Rust side reads bytes from a channel.

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
