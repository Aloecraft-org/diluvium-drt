# Python from a DRT program, and reaching it on WSL

**Status:** wiring notes, written 2026-09-06 against `v0.5.0rc3`. Cells are
marked the way `doc/Platforms.md` marks its matrix — **measured** (run on
this tree, output quoted), or **expected** (a claim from platform knowledge
nobody has run here). §8 says which is which for every claim below, because
half of this is about a Windows box no CI runner has.

**The ask.** Let a DRT session run Python, on a Windows machine that is
probably WSL.

**The answer.** Nothing needs building. `exec/run` shipped in `full` and
runs Python today; `ssh/exec` runs it on another box; `drt tunnel` reaches
a box with no inbound address. What this document is really about is §6 —
the narrowing everyone reaches for first does not narrow anything.

---

## surface block

1. Which wiring, in one table.
2. Wiring A: `exec/run`, on the box.
3. Wiring B: `ssh/exec`, into the box from elsewhere.
4. Reaching a box with no inbound address.
5. What bounds a subprocess, and what does not.
6. **The interpreter problem, and the pattern that answers it.**
7. WSL specifics.
8. Verified, and not.
9. Not this document's.

---

## 1. Which wiring

| | `exec/run` | `ssh/exec` |
|---|---|---|
| where the program runs | the same box as `drt` | another box |
| where `drt` runs | inside WSL | anywhere |
| what the scope pins | deadline, byte cap, allowed programs | host, user, key, host key, deadline, byte cap |
| credential | none — it is the same user | a private key the deployment holds and the guest never sees |
| needs on the far side | nothing | `sshd`, and a reachable address (§4) |
| bounds the interpreter? | no, and §6 is why | no, and less: there is no `allow` at all |

Run `drt` inside WSL and use `exec/run`. It is one less moving part, one
less credential, and the only wiring where `allow` exists. Reach for
`ssh/exec` when `drt` has to live somewhere else — a server, a container,
another laptop — and the Python has to be on the Windows box specifically.

---

## 2. Wiring A: `exec/run`, on the box

`deploy.json`:

```json
{
  "program": { "path": "app.dlua" },
  "caps": [ { "capability": "host:exec/run" } ],
  "connectors": {
    "exec": {
      "scope": {
        "max_timeout_ms": 3000,
        "max_output_bytes": 65536,
        "allow": ["/usr/bin/python3.11"]
      }
    }
  }
}
```

`app.dlua`, with JSON as the interchange in both directions — msgpack
carries the strings, Python parses and prints, and neither side needs a
library:

```lua
local v, status, detail = host.exec.try_run(
  { "python3", "-c",
    "import sys, json; d = json.load(sys.stdin); print(json.dumps({'n': len(d['rows']), 'max': max(d['rows'])}))" },
  { stdin = '{"rows": [3, 41, 7]}' })

if v then print(v.status, v.stdout) else print(status, detail) end
```

Measured — those two files exactly, run on this tree:

```
$ drt run --config deploy.json
drt: exec wired: granting host:exec/run leaves the sandbox (GUARANTEES.md); only the scope's deadline, output cap and allow list bound it
0	{"n": 3, "max": 41}
```

The same scope, asked for four more things (paths abridged):

```
  system python3             ok       status 0  2
  the venv's python          ok       status 0  /…/wsl/.venv
  perl (not allowed)         error    'perl' is outside this scope's allow list; a program may start only what the deployment allows
  over the deadline          error    the child was killed at the 3000 ms deadline
  over the byte cap          error    stdout passed this deployment's byte cap (65536); the child was killed, the output refused
```

Three things that line up with `connectors/exec/src/lib.rs` and are worth
knowing before you hit them:

- **A venv works, and is selected by the path you exec**, not by anything
  in the config: `.venv/bin/python3` reported the venv as its `sys.prefix`.
- **A nonzero exit is not an error.** `{status = 1}` is the child's answer,
  read the way a shell script reads `$?`; a program that is not there is
  `{status = 127}`. `error` is the call itself failing — the deadline, a
  cap, a refusal.
- **`allow` entries are absolute; the call's `argv[0]` need not be.** The
  relative `.venv/bin/python3` above resolved and matched.

---

## 3. Wiring B: `ssh/exec`, into the box from elsewhere

The scope is the place — which host, as which user, with which key,
trusting which host key. The program names only the command.

```json
{
  "program": { "path": "app.dlua" },
  "caps": [ { "capability": "host:ssh/exec" } ],
  "connectors": {
    "ssh": {
      "scope": {
        "host": "127.0.0.1:2222",
        "user": "mike",
        "key_path": "drt_to_wsl",
        "host_key": "ssh-ed25519 AAAAC3Nz… wsl",
        "timeout_ms": 10000,
        "max_output_bytes": 65536
      }
    }
  }
}
```

```lua
local v, status, detail = host.try("ssh/exec",
  { command = "python3 -c 'import sys; print(sys.version.split()[0])'" })
```

**The scope is validated at startup, by name.** This is the part that keeps
a typo from becoming a 3am auth failure, and all three paths are measured:

```
# no trust anchor
drt: 'host:ssh': scope names no trust anchor: set host_key (OpenSSH public key) or
     host_fingerprint (SHA256:...); trust-on-first-use is never the default

# key_path that is not there
drt: 'host:ssh': cannot read key file /nope/id_ed25519: No such file or directory

# a valid scope: startup passes, and the failure is the network's
ssh/exec -> error  connecting to 127.0.0.1:1: ssh protocol failure: Connection refused
```

Get the host key line off the WSL box with
`ssh-keyscan -t ed25519 localhost`, or take the `SHA256:…` fingerprint into
`host_fingerprint` instead. One of the two is required and there is no
trust-on-first-use.

**There is no `allow` here.** `ssh/exec` takes a command *string*, run by
the remote login shell, so the scope bounds where and as whom but never
what. If that matters, the narrowing is on the far side: a `command=`
restriction in `authorized_keys`, or a dedicated user whose shell is the
wrapper of §6.

---

## 4. Reaching a box with no inbound address

A machine at home has no inbound address, which is the whole subject of
`doc/Relay.md`. The relay splices an outbound leg from the box to an
outbound leg from the caller, and never looks inside; `examples/14-ssh-
through-a-tunnel` is this exact case.

On the WSL box, in front of its sshd — `--park` takes the park URL itself,
and the two keys are per label and different on purpose (`doc/Relay.md`):

```sh
drt tunnel --park "wss://rendezvous.example/park/wsl?k=$PARK_KEY" --to 127.0.0.1:22
```

and from wherever `drt` runs, the caller's half as an `ssh`
`ProxyCommand`:

```sh
ssh -o ProxyCommand="drt tunnel wss://rendezvous.example/s/wsl?k=$CALLER_KEY" mike@wsl
```

Everything ssh knows keeps working through it — host-key verification,
agent forwarding, `rsync`, `-L`/`-R` — because the pipe carries ciphertext
and nothing else.

**`ssh/exec` and the relay compose through `--local`** (issue #13). The
scope dials a `host:port`, and the caller half of the tunnel was
stdio-only, the OpenSSH `ProxyCommand` shape (`crates/drt/src/tunnel.rs`:
`stdio_to_ws`), so for a while there was nothing for `scope.host` to
point at. Now there is: beside the deployment,

```sh
drt tunnel "wss://rendezvous.example/s/wsl?k=$CALLER_KEY" --local 127.0.0.1:2222
```

binds a local port and gives each accepted connection its own fresh leg
through the relay — one claim per connection, nothing multiplexed — and
`scope.host = "127.0.0.1:2222"` is the rest. A claim the relay refuses (a
wrong key, an unknown label) closes the local connection at once, so
`ssh/exec` answers `error` rather than waiting on a half-open socket. It
is a second process to supervise, the same way `--park` is on the other
side; the in-process form (the ssh connector taking an already-open
stream) is an ego-transport seam, filed there, and `--local` loses no
code when it lands.

If the box is on your own network, skip all of it: `ssh/exec` dials the
address directly.

---

## 5. What bounds a subprocess, and what does not

**The instruction budget does not.** GUARANTEES.md says granting `exec` is
leaving the sandbox, in those words, and `drt` prints it on stderr when the
connector is wired. Three things bound it instead, all the deployment's:

- `max_timeout_ms` — a ceiling. A call may ask for less, never more. At the
  deadline the child is killed (SIGKILL, and the sweep takes the whole
  process group), and the call answers `error`.
- `max_output_bytes` — each stream, and stdin. Past it the child is killed
  and the output is **refused rather than truncated**, like every other cap
  in the tree.
- `allow` — and §6 is about how little it bounds when the program is an
  interpreter.

**One thing more, and it is a scheduling fact rather than a security one.**
`ExecConnector::call` runs its work synchronously
(`connectors/exec/src/lib.rs`), and `Pump::pump`
(`crates/drt-swarm/src/pump.rs`) polls a connector's future inline on the
drive thread with a no-op waker. A future that blocks rather than yielding
`Pending` therefore stalls **every instance in the deployment** for as long
as the child runs. The deferred pump landed in 0.5.0 and parks a call that
yields; `exec` does not yield, so it does not park.

For a 200 ms script this is invisible. For a 30-second one, the whole
deployment stands still for 30 seconds — a listener's requests included, at
which point `conn_deadline_ms` starts answering 504. So: **keep
`max_timeout_ms` well under anything else the deployment owes**, and if the
Python job is genuinely long, it wants a queue and a worker rather than a
hostcall.

`ssh/exec` is *not* the same shape, from v0.5.0rc4: its call awaits, and
the drive loops now enter a runtime for it to await on
(`crates/drt/src/runtime.rs`), so a slow remote command parks in the pump
and stalls only the instance that made it. Before rc4 it blocked exactly
as `exec` does — measured, in the same file — and the first draft of this
section said so of both. Its `timeout_ms` still defaults to 30 s, which is
now a bound on one instance's wait rather than on the deployment's.

---

## 6. The interpreter problem, and the pattern that answers it

This is the section to read if you read one.

**Allowing an interpreter is allowing arbitrary code execution as that
user.** `python3` takes `-c`. A scope of

```json
"allow": ["/usr/bin/python3.11"]
```

does not narrow `host:exec/run` at all — it removes the convenience of
other binaries and leaves the whole machine reachable through
`python3 -c "import os; …"`. The allowlist is doing exactly what it says
and what it says is not what it looks like.

**And it is wider than it looks, because the comparison resolves
symlinks.** `connectors/exec` compares after resolving both sides, so a
venv's `python3` — a symlink to the system interpreter — is the same
program as the system interpreter. Measured:

```
system python3:      /usr/local/bin/python3 -> /usr/bin/python3.11
venv python3:        .venv/bin/python3      -> /usr/bin/python3.11

allow = ["/usr/bin/python3.11"]
venv via system-only allowlist: ok  /…/wsl/.venv
```

Allowing one permitted the other, and the venv was genuinely active. There
is no config line that distinguishes them, and there should not be a
belief that there is. (The connector's own doc names the same effect for a
busybox that links every name to one binary.)

**The pattern that does narrow: allow a wrapper, not an interpreter.** Put
a script at a fixed absolute path, have it take *data* on stdin and answer
JSON, and allow the script. The guest then chooses the data and never the
code:

```python
#!/usr/bin/env python3
"""Takes data on stdin, answers JSON. The guest chooses the data, never the code."""
import sys, json
rows = json.load(sys.stdin)["rows"]
print(json.dumps({"n": len(rows), "mean": sum(rows) / len(rows), "max": max(rows)}))
```

```json
"allow": ["/opt/drt/tools/summarize"]
```

Measured — the shebang is the kernel's business, so the script is the
program `allow` matches, and the interpreter is not reachable:

```
  the wrapper, fed data        ok    {"n": 3, "mean": 5.0, "max": 9}
  python3 directly             error  'python3' is outside this scope's allow list;
                                      a program may start only what the deployment allows
```

Keep the wrapper's own dependencies pinned (a venv shebang, or
`#!/opt/drt/venv/bin/python3`), because the wrapper is now the contract and
its imports are part of it.

---

## 7. WSL specifics

**Expected, not measured** — no runner here is Windows, and `doc/Platforms.
md` marks the native-Windows column the same way. Each is the kind of thing
that costs an afternoon, so they are written down rather than rediscovered.

- **WSL is Linux, so the released binary is the binary.**
  `drt_linux_static_x86_64` from the release page runs as-is; there is no
  Windows build (`doc/Platforms.md`: not built, `full` blocked on
  cross-compiling `aws-lc-sys` through russh). `exec` in particular refuses
  to compile off unix — it is process groups and pipes — so WSL is not a
  convenience here, it is the supported path.
- **Keep the working set on the Linux filesystem.** `/mnt/c/…` crosses a
  9P/`drvfs` boundary: slow, and its permission bits are synthesized, which
  makes `allow`'s realpath comparison and the executable bit on a wrapper
  script both worth checking twice. Put the program, the wrapper and the
  venv under `~`.
- **WSL2 stops the distro when its last process exits.** A deployment meant
  to keep serving needs something holding it up — `wsl.exe -d <distro> -e
  …` from a Windows scheduled task, or a keepalive process.
- **`systemd` is opt-in.** `doc/Failure-Modes.md` wants `Restart=always`
  under a supervisor; that needs `[boot] systemd=true` in `/etc/wsl.conf`
  and a `wsl --shutdown` to take effect. Without it, supervise from the
  Windows side instead.
- **sshd, if you take wiring B**, is not running by default and is not
  installed in every image. It also cannot have port 22 if Windows'
  OpenSSH server already holds it — give WSL's sshd 2222 and be explicit.
- **WSL2's IP moves.** Its address changes across restarts, so do not pin
  it in a scope. Reach it as `localhost:2222` from Windows (WSL2 forwards
  localhost), or from elsewhere through §4's tunnel, whose label is stable
  where an IP is not.

---

## 8. Verified, and not

| claim | how |
|---|---|
| `exec/run` runs system and venv Python; JSON in and out over stdin/stdout | **measured**, output in §2 |
| `allow` refuses an unlisted program, by name | **measured**, §2 |
| the deadline kills and the byte cap refuses, with those words | **measured**, §2 |
| a venv interpreter and the system one are the same program to `allow` | **measured**, §6 |
| a wrapper script is allowlistable and hides the interpreter | **measured**, §6 |
| the `ssh` scope validates at startup: trust anchor, key file | **measured**, §3 |
| a valid `ssh` scope passes startup and fails at the network | **measured**, §3 |
| `exec` blocks the drive loop for the child's lifetime | **read**, not timed under load: `connectors/exec/src/lib.rs` runs its work synchronously and `crates/drt-swarm/src/pump.rs` polls inline with `Waker::noop()` |
| `ssh/exec` (and `rest`) park rather than block, from rc4 | **measured** for `rest` by `crates/drt/tests/start.rs::parking`: a 600 ms call, and the loop's longest pause under it is milliseconds; `ssh` takes the identical `try_current` path |
| `ssh/exec` against a real sshd | **not verified here** — no sshd in this container. The transport, auth, host-key verification, timeout and output cap are covered by `connectors/ssh/tests/exec.rs` against a real russh server; remote *execution* of Python is untested anywhere and wants your box |
| everything in §7 | **expected** |

The first thing to measure on the real box is the last row of §7 that you
depend on, and then `ssh/exec` end to end if you take wiring B.

---

## 9. Not this document's

**A `python/*` hostcall family** — a Python process answering a whole
capability over the plugin channel, rather than one subprocess per call —
is `doc/Plugins.md`. The channel is unbuilt; the protocol is upstream's,
language-neutral, and has a C fixture and a Node one. That is the route
that would give Python a *scope* of its own, and would not stall the drive
loop, because the channel is specified as a polled state machine that
yields.

Until then `exec/run` is the honest escape hatch it is documented as, and
§6 is how to make it narrow.
