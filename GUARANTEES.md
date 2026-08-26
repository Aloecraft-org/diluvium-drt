# What DRT guarantees, and what it deliberately does not

**Status:** normative, CI-checked for presence, maintained from day one
(SPEC.md §11). This list only grows deliberately: adding a row is a design
decision with a rationale, never a drive-by. Removing a caveat requires the
mitigation to have actually shipped.

A guarantee document that overstates is worse than none. Each entry below is
worded to be quotable verbatim in a security conversation.

## The C core remains C

Rust wraps the part that was never the dangerous part. The Diluvium VM — the
interpreter, the GC, the compiler — is the C core from `aloecraft-org/diluvium`,
embedded via the `dv.h` instance ABI. DRT's Rust does not make the VM
memory-safe; it makes the *host* — the swarm bookkeeping, the connectors, the
capability enforcement, the network surface — memory-safe, which is where
untrusted input actually arrives.

## The bytecode verifier does not exist yet

Treat untrusted bytecode as untrusted native code. The loader's operand checks
refuse malformed chunks rather than crashing, but Lua 5.1 shipped a fuller
checker than that and still had escapes. The mitigations are:

- `DV_FLAG_TEXT_ONLY` — refuse precompiled chunks, accept source only. Set it
  whenever the bytes did not come from your own compiler.
- The wasm-engine tier (SPEC.md §8, deferred) — untrusted bytecode inside a
  wasm sandbox is the real mitigation, and it is not built yet.

## `exec` leaves the sandbox

A subprocess runs outside the VM, so the instruction budget cannot bound it —
it is bounded by a wall-clock timeout and an output cap, host-side, and by
nothing else. Granting `exec` is leaving the sandbox; the connector is behind
a loud flag because enabling it must be a conscious act. Do not read any other
guarantee in this file as covering what an `exec`'d process does. `host:ssh/exec`
is the same caveat on another machine: the scope pins where, as whom, with
which key, and under which host key — but what the command does there is
outside everything this file promises.

## `sshd` is a deliberate front-door exposure

Running `drt serve` with a transport listener publishes a network surface on
purpose. It is pubkey-only, modern-suite-only (ed25519, curve25519,
chacha20-poly1305, strict kex), built on russh rather than hand-rolled — but a
listening socket is a listening socket. An SSH principal is an attenuated node
in the capability tree; the ceiling on what a connecting key can do is the
grant set mapped to it in root config, and reviewing that mapping is part of
deploying.

## tokio introduces scheduling nondeterminism

The drive loop is structured around an event log (which-queue-fired decisions
and hostcall replies — the two nondeterminism sources) so that replay is a
later feature flag rather than a rewrite. But replay is **not implemented**,
and until it is, no run of `drt` is reproducible: the tokio scheduler orders
wakeups differently every run. Do not advertise replay until it exists.
