# 02-capabilities

One program, two apps. A drt app is a config plus a program; here the
program is the same file both times and only the config around it changes,
so every difference in the output is the deployment's doing.

## Run it

```
cd examples/02-capabilities
drt run app.dlua
drt run --config with-fs.json
```

Paths inside a config resolve against the directory you run from, so start
with the `cd`. `with-fs.json` names `./workspace`, the directory next to it.

## What you should see

```
$ drt run app.dlua
  fs/read note.txt          denied  no connector is wired for 'fs/read' in this process
  fs/read ../../etc/passwd  denied  no connector is wired for 'fs/read' in this process
  sql/query                 denied  no connector is wired for 'sql/query' in this process

$ drt run --config with-fs.json
  fs/read note.txt          ok      A file inside the granted directory.
  fs/read ../../etc/passwd  error   '../../etc/passwd' resolves outside the granted scope; a program names files within what the host granted, and nothing beyond it
  sql/query                 denied  'sql/query' is outside this instance's grants
```

Both runs exit 0 and write nothing. The output is the same every time.

## What it teaches

**The program did not change; the app did.** `app.dlua` makes the same three
calls in both runs. Nothing in it asks which deployment it is in, and there
is no branch anywhere on `if fs is available`. It calls, and reads what came
back. That is what makes one program deployable into deployments that wire
different things.

**A config grants a place, not a filename.** `with-fs.json` says the `fs`
connector may use `./workspace`, and says nothing about `note.txt`.
`app.dlua` says `note.txt`, and says nothing about `workspace`. The two
halves meet at the connector, which is why the same program can be pointed at
another directory by editing the config and nothing else.

**`..` buys nothing.** The escape attempt is refused against the path the
filesystem resolves to, not the string that was typed, so climbing out with
`..` fails and so does a symlink inside the scope that points out of it.

**There are two layers that can say no, and they say it differently.**
`denied` is the gate's word: the call was refused before any connector saw
it, either because nothing is wired for that family (the first run) or
because the grant ceiling in `caps` does not cover it at all (`sql/query` in
the second run — the config grants `host:time` and `host:fs/*`, and that is
the ceiling for the whole process). `error` means the call did reach a
connector and the connector refused it against its own scope, which is the
escape attempt above. Both arrive as replies, with a sentence you can print.

**A refusal is a reply, not an exception.** `host.try(name, args)` returns
`value, status, detail`, so all three outcomes -- a result, a scope refusal,
and a family that was never wired -- are read by the same three lines of
code. The direct forms (`host.fs.read`, `host.call`) raise instead, and are
the right shape only where a refusal really would be a bug.
