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
it is bounded by a wall-clock timeout, an output cap and an allow list of
programs, host-side, and by nothing else. Granting `exec` is leaving the
sandbox, so enabling it is a conscious act three times over: the connector is
in the `full` build only, it is wired only when a config names
`connectors.exec`, and `drt` announces the wiring on stderr before the first
step. Do not read any other guarantee in this file as covering what an
`exec`'d process does. `host:ssh/exec`
is the same caveat on another machine: the scope pins where, as whom, with
which key, and under which host key — but what the command does there is
outside everything this file promises.

## The signing key is out of a guest's reach, not out of the machine's

`host:crypto/*` gives a program the right to *ask* for a signature. The key
is never in the reply, never in the guest heap, and never in a snapshot, and
the two working subkeys are derived so that a `crypto/hmac` grant cannot be
used as an oracle to forge a JWT. That is the whole of the promise, and it is
about the **guest boundary**.

It is not about the host. The key sits in this process's memory for the
process's life, and if the config names it inline it sits in a file on disk.
A grant that can read that file (`host:fs/read` scoped to the config's
directory) or run a command on the box (`exec`, `host:ssh/exec`) recovers the
key, and no amount of care inside the connector changes that — the scopes
are the control. Prefer `key_file` or `key_env` over an inline `key`, and do
not grant a program a scope containing either.

Two narrower limits, stated rather than implied. The constant-time compares
cover the MAC verdicts (`jwt_verify`, `crypto/hmac` with `expect`) and
nothing else claims side-channel resistance. And `crypto/turn_credential`
signs with **HMAC-SHA1** under the **raw** configured secret, because
coturn's `use-auth-secret` scheme fixes both; it is the one call that does
not use a derived subkey, and the primitive is there for interop, not
because it was chosen.

## `sshd` is a deliberate front-door exposure

Running `drt serve` with a transport listener publishes a network surface on
purpose. It is pubkey-only, modern-suite-only (ed25519, curve25519,
chacha20-poly1305, strict kex), built on russh rather than hand-rolled — but a
listening socket is a listening socket. An SSH principal is an attenuated node
in the capability tree; the ceiling on what a connecting key can do is the
grant set mapped to it in root config, and reviewing that mapping is part of
deploying.

## An SSH server in a page is reachable from outside the tab

The `web` build carries one (`DrtSshServer`), and a page that parks a leg on a
rendezvous relay can be reached by anyone holding that label's caller key —
from any machine, not only the one the tab is open on. That is the capability
and it is the point; this entry is where it stops.

Nothing listens by default, and no default opens the door. A page must supply
a host key, supply a list of authorized keys, and pump a socket; the class
being present in the module is not a listener. There is no "accept any key" —
`Authorized` is a list of `authorized_keys` lines, and an empty one
authenticates nobody rather than everybody. There is no password method at
all.

A session can do whatever the page's shell exposes, and that is the whole of
the grant. With `drt-term.js` attached it is every `drt` verb the build
carries, `drt repl --unsafe` included. Choosing which shell sits behind the
session is the security decision; there is no second one behind it.

It is not a way onto the machine, and that part is not ours to promise or to
break: the `web` build's filesystem backend is an empty `MemFs`, `cfg`'d for
the browser target, and a browser has no other. A session reaches at most
what the page seeded, and only where the page's config wires `fs` at all — a
config-less run has no `fs` connector to call. Nor is it a way into the page:
the shell runs `drt`, not JavaScript, and no connector in this build can
execute script or reach the document. What the session does see of the page
is the terminal — the same sink the page's own output goes to, which is what
a terminal is.

It is only as strong as the page. The host key and the authorized list live
wherever the page keeps them, which is readable by anything that can run
script on that origin: an XSS on the page is the page's SSH server. The host
key must also persist across reloads — one regenerated on load makes every
client see a changed host key, which trains whoever connects to click through
the warning that is supposed to matter.

The relay decides reachability, not secrecy. SSH is end-to-end between the
client and the page, so a relay on the path carries ciphertext and can drop a
connection but not read it. What the caller key gates is who can reach the
label at all; it is a reachability control, not a second authentication.

## tokio introduces scheduling nondeterminism

The drive loop is structured around an event log (which-queue-fired decisions
and hostcall replies — the two nondeterminism sources) so that replay is a
later feature flag rather than a rewrite. But replay is **not implemented**,
and until it is, no run of `drt` is reproducible: the tokio scheduler orders
wakeups differently every run. Do not advertise replay until it exists.
