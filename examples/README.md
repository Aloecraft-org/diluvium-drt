# DRT examples

A run-through for someone who has a `drt` binary on their PATH and has never
run anything with it. Seven sittings, meant in order: one idea each, a
command block you can paste, and an `expected.txt` that is the
real output of running it rather than a transcription of what it ought to
say. This is not a reference — it is the shortest path to being oriented. A
**drt app** is a config plus a program, and the first two examples are that
sentence taken apart; most of the rest are one program run under two or
three configs, so that every difference between the outputs is the config's
doing and not the program's. Everything here is v0.4.0.

| directory | what it teaches | the command |
|---|---|---|
| [`00-install-methods`](00-install-methods) | Getting a binary, then asking it what it is with `buildinfo` instead of trusting the filename. | `drt buildinfo` |
| [`01-hello`](01-hello) | A single `.dlua` file is a runnable app. With no config, `time` answers and every other family replies `denied`. | `drt run app.dlua` |
| [`02-capabilities`](02-capabilities) | One program, two apps. What a config wires, what it does not, and why `denied` and `error` are different words. | `drt run app.dlua`, then `drt run --config with-fs.json` |
| [`03-writing-dlua`](03-writing-dlua) | The language: what `drt run` seals off, the additions to Lua, and why the clock is `host.time()` and not `time.now()`. | `drt run app.dlua` |
| [`04-files`](04-files) | `fs/read`, `fs/write`, `fs/list`, `fs/remove` against one granted directory, and the four separate things a scope constrains. | `drt run --config readwrite.json`, then `drt run --config read-only.json` |
| [`05-calling-a-rest-api`](05-calling-a-rest-api) | The `rest` scope is an origin allowlist, and a URL outside it is refused by name before an address is looked up. | `drt run --config allowlist.json` |
| [`05-calling-a-rest-api-live`](05-calling-a-rest-api-live) | The other half: the URL the allowlist permits, over a real socket, carrying a header the deployment injects and the program can neither set nor read. Needs a network. | `drt run --config allowlist.json` |
| [`06-budgets`](06-budgets) | What a budget bounds (VM instructions, VM memory), what it does not (wall time, spawns), and what running out looks like from outside. | `drt run app.dlua`, then `--config bounded.json`, then `--config tight.json` |
| [`08-spawn-and-hibernation`](08-spawn-and-hibernation) | A program starting another: a child holds a subset of its parent's grants and nothing more, and parks itself when it has nothing to do. | `drt start --config app.json` |

## If you only do three

`01`, `02`, `04`. `01` is the cold start and the one idea the rest rests on —
a refusal is a reply, not an exception. `02` is that idea with a config
around it, so you see the deployment decide. `04` is a real grant doing real
work, with a directory that outlives the process. Add `03` before you write a
program of your own; it is the hour it saves you.

## Running one

```
cd examples/02-capabilities
drt run app.dlua
```

Every directory is self-contained. It runs from inside itself, and it reads
and writes nothing outside itself. That is deliberate: paths inside a config
resolve against the directory you are standing in, so a config naming
`./workspace` means a different place if you run it from somewhere else —
start with the `cd` and the output matches. Where you do see a `..` in one of
these examples, in `02` and `04`, it is a path the run refuses, which is the
whole point of the line. The one example that writes files, `04`, rewrites
them on every run and lists them in its own `.gitignore`, so running it twice
prints what it printed the first time and a fresh checkout prints it too.

Each directory's `meta.json` carries the exact command the gate runs, along
with the sed expressions that normalise the few lines that legitimately
differ between runs — a clock reading in `01`, a line number in `03`.
Everything else is the same text every time.

## Running all of them

```
cd examples
./run-all.sh          # skips the examples that need a network
./run-all.sh --net    # includes them
```

It runs each directory's `cmd`, applies that directory's `normalise`, and
diffs against `expected.txt`. One directory sets `needs_network: true` —
`05-calling-a-rest-api-live`, the one that opens a socket — so without
`--net` it is printed as skipped and named again in the summary, never
counted as a pass. One run inside `06` exits 1 on purpose; that is part of
what it is showing, and part of its expected output.

## Not covered yet

Named rather than omitted, so you are not left looking for them.

- **`drt start`.** Every command in the tour is `drt run`, which runs one
  program to completion and exits. `drt start` runs the deployment — the
  root program, its swarm, and whatever listeners the config names — in the
  foreground, and it is what a listener or a long-lived swarm wants. Nothing
  here needs it, because everything here is one program that finishes.
- **`drt repl`** starts and works, and has no line editor: no history, no
  arrow keys, no editing a line you have already typed. That is not a thing
  to put in front of someone on their first day, so the tour does not.
- **`drt ps`** is a stub. It prints `drt ps: not built yet` and a sentence
  saying it reaches a running deployment over the control endpoint, which
  lands with sshd. There is nothing to demonstrate.
- **Running a local process.** There is no example because there is nothing
  to run: DRT has no local process execution at all. There is no `exec`
  family — the name appears nowhere in the binary, no connector implements
  one, and no config can wire one, so a call to it is refused the way any
  unrecognised name is rather than by a rule written for it. A drt app
  cannot shell out. The nearest thing that exists is `ssh/exec`, which runs
  a command on a host the config named, through a connector, at the cost of
  a grant. There is no example for it yet.
- **The browser tier.** The same swarm runs in a page over a JS host bridge
  (`doc/Browser.md`). Nothing here touches it; these examples are a terminal
  and a binary.
- **An outbound call to somewhere of your own.** `05-calling-a-rest-api-live`
  reaches one origin, GitHub's, because it has to name one. Pointing it at
  your own API is an edit to `allowlist.json` and nothing else.

## Still to come

The tour stops at `06`. These are the sittings that follow, and the reason
the numbering has room above it:

- **sql** — a granted database, `sql/query` and `sql/exec`, and a scope that
  is a file rather than an origin.
- **spawn and hibernation** — `host.spawn`, a child holding a strict subset
  of its parent's grants, and an instance that sleeps until a message wakes
  it.
- **netcheck** — `drt netcheck`, and what the network in front of you will
  and will not carry.
- **ssh** — `ssh/exec` against a host the config named.
- **tunnel and relay** — [`rendezvous/`](rendezvous) is this one today: the
  relay, its supervisor, `drt tunnel` on both ends, and ssh over the top. It
  needs a machine on either side and a key you generate, so it is not
  numbered or gated yet; it takes a number here once it is self-contained the
  way the rest are.
- **under the hood** — the raw `host`/`calls` queue pair that every call in
  these examples sits on top of.

`hello.dlua`, `by-hand.dlua`, `deployment.json` and `workspace/` sit loose
beside the numbered directories, predate them, and are what the repository
README links. They cover the same ground as `01` and `02`, and their paths
are written from the repository root rather than from here, so they run from
there — which is the thing the numbered examples were built to avoid. Prefer
the numbered ones.
