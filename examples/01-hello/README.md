# 01-hello

The cold start. One `.dlua` file, no config, nothing installed beyond the
`drt` binary.

## Run it

```
cd examples/01-hello
drt run app.dlua
```

## What you should see

```
A single .dlua file is a drt app. This one is running, with no config.

answered, with no config at all:
  time             1788282276565
  time/monotonic   0

denied, because wiring a connector is a config's job:
  crypto/random    denied  no connector is wired for 'crypto/random' in this process
  fs/read          denied  no connector is wired for 'fs/read' in this process
  sql/query        denied  no connector is wired for 'sql/query' in this process

pure, so it asks the host for nothing:
  time.iso(0)      1970-01-01T00:00:00Z
```

The two values in the first block come from the run that produced this text;
yours will differ. Everything else matches line for line. The program writes
no files and exits 0.

## What it teaches

**A drt app is a config plus a program, and the config is optional.**
`app.dlua` is an app with that half left out: `drt run` takes the program
directly, so one file you can read in a minute is already a thing you can
run. There is no project to create and no build step in front of it.

**What a program can reach is the deployment's decision, not the program's.**
With no config, two hostcall families answer: `time` and `time/monotonic`.
Every other family answers

```
denied  no connector is wired for 'crypto/random' in this process
```

**A denial is a reply, not an error.** That is the part worth slowing down
for. The program asked for entropy, the process answered "nobody here does
that", and the next line ran. Nothing was dropped, nothing timed out, and
there was no exception to catch. `host.try(name, args)` returns
`value, status, detail`, so the program reads a refusal through the same code
path it reads a result through -- which is what lets one program stay honest
across deployments that wire different things.

The direct forms (`host.time()`, `host.fs.read(...)`) raise on a non-ok
status instead, and are the right shape where a refusal really would be a
bug. `app.dlua` uses each where it fits.

**The clock is a capability; the calendar is not.** `host.time()` costs a
grant and is answered by a connector, so it lands in the log and a replay
replays the recorded moment. `time.iso` is arithmetic over a number you
already have, so it asks the host for nothing. There is no `time.now()`, and
now you know why.

Wiring a connector so that `fs/read` answers is what a config does, and that
is the next example.
