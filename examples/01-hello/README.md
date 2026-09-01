# 01-hello

The cold start: one `.dlua` file, no config, nothing but the `drt` binary.

## Run it

```
cd examples/01-hello
drt run app.dlua
```

## What you should see

```
A single .dlua file is a drt app. This one is running, with no config.

answered, with no config at all:
  time             1788287486813
  time/monotonic   0

denied, because wiring a connector is a config's job:
  crypto/random    denied  no connector is wired for 'crypto/random' in this process
  fs/read          denied  no connector is wired for 'fs/read' in this process
  sql/query        denied  no connector is wired for 'sql/query' in this process
```

The clock reading is from the run that made this text; yours will differ. The
program writes no files and exits 0.

## What it teaches

**A drt app is a config plus a program, and the config is optional.** `drt
run` takes the program directly, so one file you can read in a minute is
already something you can run. Nothing to create, nothing to build.

**What a program can reach is the deployment's decision, not the program's.**
With no config one connector is wired — `time` — and it answers two calls,
`time` and `time/monotonic`. Nothing else is wired, so every other call is
refused by the gate before a connector sees it.

**A refusal is a reply, not an exception.** The program asked for entropy, the
process answered "nobody here does that", and the next line ran. Nothing was
dropped and there was nothing to catch: `host.try(name, args)` returns
`value, status, detail`, so a refusal is read through the same code path as a
result — which is what lets one program run unchanged where different things
are wired.

Making `fs/read` answer is what a config does, and that is `02-capabilities`.
