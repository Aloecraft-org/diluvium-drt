# 04-files

Four verbs — `fs/read`, `fs/write`, `fs/list` and `fs/remove` — against one
directory, and the four separate things a scope constrains.

One program, two drt apps. An app is a config plus a program: `app.dlua` is
the same file in both runs, `readwrite.json` and `read-only.json` differ in a
single word, so every difference between the two outputs is the deployment's
doing.

## Run it

```
cd examples/04-files
drt run --config readwrite.json
drt run --config read-only.json
```

Paths in a config resolve against the directory you run from, so start with
the `cd`. Both configs name `./workspace`, the directory beside them.

Run them in that order. The first writes `workspace/report.txt`, and the
second reads it back. That is not a quirk of the example: a directory is
state that outlives the process, which is most of the reason to grant one.

## What you should see

```
$ drt run --config readwrite.json
the four verbs, against the directory the config granted:
  fs/write report.txt      ok      wrote 80 bytes
  fs/write scratch.txt     ok      wrote 40 bytes
  fs/list .                ok      note.txt  report.txt  scratch.txt
  fs/read report.txt       ok      Written by app.dlua. The config named the directory; the program named the file.
  fs/remove scratch.txt    ok      removed
  fs/list .                ok      note.txt  report.txt

what the scope refuses, and in whose words:
  fs/write big.txt         error   writing 'big.txt' would reach 4096 bytes, past the 1024-byte cap this scope allows
  fs/read ../app.dlua      error   '../app.dlua' resolves outside the granted scope; a program names files within what the host granted, and nothing beyond it
  fs/read /etc/hosts       error   '/etc/hosts' is absolute; name a path inside the granted scope

$ drt run --config read-only.json
the four verbs, against the directory the config granted:
  fs/write report.txt      error   'fs/write' needs access = "readwrite"; this scope is read-only
  fs/write scratch.txt     error   'fs/write' needs access = "readwrite"; this scope is read-only
  fs/list .                ok      note.txt  report.txt
  fs/read report.txt       ok      Written by app.dlua. The config named the directory; the program named the file.
  fs/remove scratch.txt    error   'fs/remove' needs access = "readwrite"; this scope is read-only
  fs/list .                ok      note.txt  report.txt

what the scope refuses, and in whose words:
  fs/write big.txt         error   'fs/write' needs access = "readwrite"; this scope is read-only
  fs/read ../app.dlua      error   '../app.dlua' resolves outside the granted scope; a program names files within what the host granted, and nothing beyond it
  fs/read /etc/hosts       error   '/etc/hosts' is absolute; name a path inside the granted scope
```

Both runs exit 0, and the pair prints the same text every time you run it.
`report.txt` is rewritten rather than appended to and `scratch.txt` is
removed before the run ends, so nothing accumulates in `workspace/` and the
listings stay put. Both created files are in `.gitignore`; the program makes
them, so a fresh checkout that has never been run still produces exactly the
text above.

## What it teaches

**The config names a directory, the program names files, and neither half
carries the other's.** Nothing in `app.dlua` says `workspace`, and nothing in
either config says `report.txt`. They meet at the connector. That split is
what lets the same program be aimed at a different directory by editing one
line of config, and it is why a config travels between machines while a
program travels between deployments.

**`access` decides which verbs exist at all.** `readwrite` wires all four;
`read` wires two, and `fs/write` and `fs/remove` refuse by name. It defaults
to `read` when the config says nothing, because a connector that granted
writes for want of being asked would be the wrong default in the one place
it matters.

That check runs before any other, which is why `fs/write big.txt` reports the
read-only scope in the second run rather than the size cap. It never got as
far as the size.

**`max_bytes` is the only bound there is on a file.** It caps a single file
in either direction, host-side, and it is checked before the write — so
`big.txt` is refused and never created. A guest cannot bound its own file
sizes and the instruction budget does not reach the filesystem, so nothing
else is watching. It defaults to 1 MiB; these configs say 1024 so the
refusal fits on the page.

**`..` buys nothing, and neither does a leading `/`.** `..` is folded before
the filesystem is touched at all, and `fs/read`, `fs/list` and `fs/remove`
then check a second time against the path the filesystem actually resolved
to — so a name that only reaches outside by way of a symlink is refused as
well. `fs/write` is the exception worth knowing: it resolves the *containing
directory* rather than a file it may be about to create, so it catches a
symlinked directory but follows a symlink sitting at the final name. Grant
`readwrite` over a directory someone else can drop links into with that in
mind. The program cannot read its own source, one level above the directory
it was granted — and `app.dlua` is a file in this very example directory,
which is the point: the grant is `workspace`, not "wherever the app lives".
The absolute path is refused earlier still, before the filesystem is touched
at all, so nothing here reads `/etc/hosts` or needs it to be there.

**Every refusal here says `error`, not `denied`.** `denied` is the gate's
word for a call that never reached a connector. These calls all reached the
`fs` connector, which then refused them against its own scope, and each
reply carries the sentence that connector wrote. `02-capabilities` has the
other half of that distinction.
