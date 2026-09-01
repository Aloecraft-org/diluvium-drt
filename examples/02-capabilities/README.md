# 02-capabilities

One program, two apps. A drt app is a config plus a program; here the program
is the same file both times and only the config around it changes, so every
difference in the output is the deployment's doing.

## Run it

```
cd examples/02-capabilities
drt run app.dlua
drt run --config with-fs.json
```

Paths inside a config resolve against the directory you run from, so start
with the `cd`. `with-fs.json` names `./workspace`, the directory beside it.

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
calls in both runs. Nothing in it asks which deployment it is in, and there is
no branch on `if fs is available`. It calls, and reads what came back.

**A config grants a place, not a filename.** `with-fs.json` names
`./workspace` and never says `note.txt`; `app.dlua` names `note.txt` and never
says `workspace`. They meet at the connector, which is why the same program
can be aimed at another directory by editing the config alone.

**Two layers can say no, and they say it differently.** `denied` is the gate's
word for a call that never reached a connector: nothing is wired for that
family, or `caps` — the ceiling for the whole process — does not cover it.
`error` means a connector saw the call and refused it against its own scope,
decided on the path as typed, before the filesystem is touched, so its
sentence is the same whether or not `/etc/passwd` is there. A verb a wired
connector does not have — `fs/chmod`, say — is `error` too, and its sentence
names the four it does answer.

**A refusal is a reply, not an exception.** `host.try(name, args)` returns
`value, status, detail`, so all three outcomes are read by the same three
lines. The direct forms (`host.fs.read`, `host.call`) raise instead, and are
right only where a refusal really would be a bug.

`04-files` is the same connector with a writable scope and all four verbs.
