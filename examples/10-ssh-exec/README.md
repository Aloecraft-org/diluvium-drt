# 10-ssh-exec

A command run on a machine that is not this one. `ssh/exec` is the one call
that leaves the sandbox, and the scope is what keeps it aimed: one host, one
user, one key, one trust anchor. The program names the command and nothing else.

## Run it

```
cd examples/10-ssh-exec
drt run --config deploy.json
drt run app.dlua
```

`deploy.json` is the shape a real deployment has. It stops here because the key
it names is yours and is not in this directory, which is the useful thing to
watch it do.

## What you should see

```
$ drt run --config deploy.json
drt: 'host:ssh': cannot read key file deploy_key: No such file or directory (os error 2) (expected {host, user, key_path, host_key|host_fingerprint, timeout_ms?, max_output_bytes?})
exit 1

$ drt run app.dlua
  ssh/exec  denied  no connector is wired for 'ssh/exec' in this process
  exec/run  denied  no connector is wired for 'exec/run' in this process
exit 0

```

Those two `denied` lines are replies the program read and printed. The
first run never started a program at all.

## What it teaches

**The scope is the destination.** `app.dlua` says `command = "uptime"` and
never says where. The host, the user and the key live in `deploy.json`, so a
program holding `host:ssh/exec` cannot pick another box, log in as somebody
else, or reach for a different key. It is all checked at startup, by name: a
wrong key path is a refusal at boot, not an auth failure at 3am.

**Trust is written down, never offered.** The scope must carry `host_key` (an
OpenSSH public key line) or `host_fingerprint` (`SHA256:…`) — drt prints that
requirement above as `host_key|host_fingerprint`. Take the fingerprint out of
`deploy.json` and it never reaches the key file: *scope names no trust anchor
… trust-on-first-use is never the default*. The prompt `ssh` gives you on a
first connection is a decision, and a config is where decisions live.

**Local exec is a different door.** `exec/run` comes back `denied` here for
the same reason `ssh/exec` does: nothing wired it. It exists, in the `full`
build, and [`16-exec`](../16-exec) is the app that wires it, with a scope
that names which programs, for how long, and how much output. If you arrived
from diluvium-host looking for it, that is where it went.

**The budget cannot reach it.** An instruction budget stops at this machine's
VM, so what limits a remote command is `timeout_ms` and `max_output_bytes`, and
past either the call errors. An `ok` reply is `{exit, stdout, stderr}`, with
`exit` absent rather than faked if the channel closed without one.

Everything this directory runs is the config half, and that half is real. The
call itself wants a reachable sshd — and in v0.4.0 it gets nowhere near one:
`ssh/exec` reaches for a tokio reactor no guest loop runs, and the process
panics instead of answering.

## If you wanted a shell, not a hostcall

`ssh/exec` is one connection, one command, output back as data — no session,
no PTY, a fresh handshake per call. To use your own `ssh` client through DRT,
with `scp`, `rsync` and port forwarding, see
[`14-ssh-through-a-tunnel`](../14-ssh-through-a-tunnel).
