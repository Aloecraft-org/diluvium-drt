# DRT examples

A run-through for someone who has a `drt` binary and has never run anything
with it. Thirteen sittings — fourteen directories, because `05` has a live
twin — meant in order: one idea each, a command block you can paste, and an
`expected.txt` that is the real output of running it rather than a
transcription of what it ought to say. A **drt app** is a config plus a
program; the first two examples are that sentence taken apart, and most of the
rest are one program run under two or three configs, so every difference
between the outputs is the config's doing and not the program's. This is not a
reference. Everything here is v0.4.0.

| directory | what it teaches | the command |
|---|---|---|
| [`00-install-methods`](00-install-methods) | Getting a binary, then asking it what it is with `buildinfo` instead of trusting the filename. | `drt buildinfo` |
| [`01-hello`](01-hello) | A single `.dlua` file is a runnable app. With no config, `time` answers and every other family replies `denied`. | `drt run app.dlua` |
| [`02-capabilities`](02-capabilities) | One program, two apps. What a config wires, what it does not, and why `denied` and `error` are different words. | `drt run --config with-fs.json` |
| [`03-writing-dlua`](03-writing-dlua) | The language: what `drt run` seals off, the additions to Lua, and why the clock is `host.time()` and not `time.now()`. | `drt run app.dlua` |
| [`04-files`](04-files) | `fs/read`, `fs/write`, `fs/list`, `fs/remove` against one granted directory, and the four separate things a scope constrains. | `drt run --config readwrite.json` |
| [`05-calling-a-rest-api`](05-calling-a-rest-api) | The `rest` scope is an origin allowlist, and a URL outside it is refused by name before an address is looked up. | `drt run --config allowlist.json` |
| [`05-calling-a-rest-api-live`](05-calling-a-rest-api-live) | The other half: the URL the allowlist permits, over a real socket, carrying a header the deployment injects and the program can neither set nor read. Needs a network. | `drt run --config allowlist.json` |
| [`06-budgets`](06-budgets) | What a budget bounds (VM instructions, VM memory), what it does not (wall time, spawns), and what running out looks like from outside. | `drt run --config tight.json` |
| [`07-sql`](07-sql) | `sql/query` reads and `sql/exec` writes; the scope is a directory, and the database is a name the program picks inside it. | `drt run --config readwrite.json` |
| [`08-spawn-and-hibernation`](08-spawn-and-hibernation) | A program starting another: a child holds a subset of its parent's grants and nothing more, and parks itself when it has nothing to do. | `drt start --config app.json` |
| [`09-netcheck`](09-netcheck) | One of four verdicts about the network in front of you, and the measurements that produced it. | `drt netcheck` |
| [`10-ssh-exec`](10-ssh-exec) | `ssh/exec` is the one call that leaves the sandbox, so the scope pins the destination. And there is no local `exec`, in any build. | `drt run --config deploy.json` |
| [`11-tunnel-and-relay`](11-tunnel-and-relay) | Two machines that cannot reach each other both dial out to a relay, which splices their legs into one pipe that ssh rides over. | `drt tunnel` |
| [`12-under-the-hood`](12-under-the-hood) | What every `host.*` call is underneath: the `host/calls` and `host/replies` pair, a token the host echoes back, and a reply of four fields. | `drt run app.dlua` |
| [`13-stun-server`](13-stun-server) | Run two STUN binding servers and classify this machine's NAT from what they answer. One server is never enough. | `./demo.sh` |

`drt run` executes one program to completion and exits — no swarm, no
listeners, no second instance — and it is what `01`–`07`, `10` and `12` use.
`drt start` runs the whole deployment, and `08` needs it because a child has
nowhere to live under `drt run`. `00`, `09` and `11` call neither: `buildinfo`,
`netcheck`, `tunnel` and `relay` are verbs beside those two, not apps.

## If you only do three

`01`, `02`, `04`. `01` is the cold start and the one idea the rest rests on —
a refusal is a reply, not an exception. `02` is that idea with a config around
it, so you see the deployment decide. `04` is a real grant doing real work,
with a directory that outlives the process. Add `03` before you write a
program of your own; it is the hour it saves you.

## Running them

```
cd examples/02-capabilities && drt run app.dlua   # one
cd examples && ./run-all.sh                       # all, skipping the networked one
cd examples && ./run-all.sh --net                 # all
```

Every directory is self-contained: it runs from inside itself and touches
nothing outside itself. Start with the `cd` — paths inside a config resolve
against the directory you are standing in, so a config naming `./workspace`
means somewhere else if you run it from somewhere else. Where you do see a
`..`, in `02` and `04`, it is a path the run refuses.

Two examples write a file: `04` removes what it wrote before it exits, and
`07` leaves its database on disk and drops the table at the top of every run.
Both list what they write in their own `.gitignore`, so a second run and a
fresh checkout print what the first run printed.

`run-all.sh --help` is the gate in full — what `meta.json` holds, why
`05-calling-a-rest-api-live` is skipped without `--net` and never counted as a
pass, and how the `normalise` expressions cover the few lines that legitimately
differ: a clock reading in `01` and `12`, a line number in `03`, and the one
line in `05-calling-a-rest-api-live` that depends on your network. Runs inside
`06`, `09`, `10` and `11` exit 1 on purpose, and that is in their expected
output.

## Not covered

Named rather than omitted, so you are not left looking for them.

- **Running a local process.** DRT has none. There is no `exec` family: no
  connector implements one, so a config naming it is refused at load the way
  any unrecognised connector name is, and `exec/run` is `denied` the way any
  unwired family is. A drt app cannot shell out. The nearest thing is
  `ssh/exec`, and `10-ssh-exec` is both halves of it.
- **`crypto`.** In every build's connector list, and the family `01` uses to
  show you a `denied` — and no example wires it, because in v0.4.0 the crypto
  scope demands a signing key even for the keyless calls. That conflict is
  unresolved, and it is why `crypto/random` is not answered with no config.
- **`listen`, and `drt stun`.** The inbound half: a config that answers
  requests rather than making them. `11` is as close as the set gets.
- **`drt repl`** works and has no line editor — no history, no arrow keys, no
  editing a line you have typed. Not a first day's tool.
- **`drt ps`** is a stub: it prints `drt ps: not built yet`, and reaching a
  running deployment over the control endpoint lands with sshd.
- **The relay's control plane.** `11` is the tunnel and the relay themselves.
  The half only `drt start` has — the supervisor, the admit question asked
  before a leg proceeds, the STUN pair — is in [`rendezvous/`](rendezvous),
  which wants a machine on either side and so is neither numbered nor gated.
- **The browser tier.** The same swarm runs in a page over a JS host bridge
  (`doc/Browser.md`). These examples are a terminal and a binary.
- **Your own API.** `05-calling-a-rest-api-live` reaches GitHub's because it
  has to name one origin. Pointing it elsewhere is an edit to
  `allowlist.json` and nothing else.

`hello.dlua`, `deployment.json` and `workspace/` sit loose beside the numbered
directories, predate them, and are what the repository README links. They
cover the same ground as `01` and `02` with paths written from the repository
root, so they run from there — the thing the numbered examples were built to
avoid. Prefer the numbered ones.
