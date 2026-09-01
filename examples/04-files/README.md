# 04-files

The four `fs` verbs against one granted directory, and the four things a
scope constrains. One program, two apps: `readwrite.json` and
`read-only.json` differ in a single word.

## Run it

```
cd examples/04-files
drt run --config readwrite.json
drt run --config read-only.json
```

Paths inside a config resolve against the directory you run from, so start
with the `cd`. Both configs name `./workspace`, the directory beside them.

## What you should see

```
$ drt run --config readwrite.json
the four verbs, against the directory the config granted:
  fs/write scratch.txt     ok      -
  fs/list .                ok      note.txt  scratch.txt
  fs/read note.txt         ok      A file this example ships with.
  fs/remove scratch.txt    ok      -
  fs/list .                ok      note.txt

what the scope refuses, and in whose words:
  fs/write big.txt         error   writing 'big.txt' would reach 4096 bytes, past the 1024-byte cap this scope allows
  fs/read ../app.dlua      error   '../app.dlua' resolves outside the granted scope; a program names files within what the host granted, and nothing beyond it
  fs/read /etc/hosts       error   '/etc/hosts' is absolute; name a path inside the granted scope

$ drt run --config read-only.json
the four verbs, against the directory the config granted:
  fs/write scratch.txt     error   'fs/write' needs access = "readwrite"; this scope is read-only
  fs/list .                ok      note.txt
  fs/read note.txt         ok      A file this example ships with.
  fs/remove scratch.txt    error   'fs/remove' needs access = "readwrite"; this scope is read-only
  fs/list .                ok      note.txt

what the scope refuses, and in whose words:
  fs/write big.txt         error   'fs/write' needs access = "readwrite"; this scope is read-only
  fs/read ../app.dlua      error   '../app.dlua' resolves outside the granted scope; a program names files within what the host granted, and nothing beyond it
  fs/read /etc/hosts       error   '/etc/hosts' is absolute; name a path inside the granted scope
```

Both runs exit 0. `scratch.txt` is removed before the run ends, so the
directory is left as it was found and the output is the same every time.

## What it teaches

**`access` decides which verbs exist.** `readwrite` wires all four; `read`
wires `fs/read` and `fs/list`, and the other two refuse by name. It defaults
to `read`, and it is checked first — which is why `fs/write big.txt` reports
the read-only scope in the second run rather than the size cap.

**`max_bytes` is the only bound there is on a file.** It caps one file in
either direction, host-side, and it is checked before the write, so
`big.txt` is refused and never created. It defaults to 1 MiB; these configs
say 1024 so the refusal fits on the page.

**`..` buys nothing, and neither does a leading `/`.** The grant is
`workspace`, not "wherever the app lives", so the program cannot read its
own source one level up, and an absolute path is refused before the
filesystem is touched at all. Both come back `error` rather than `denied`:
they reached the `fs` connector, which refused them against its own scope.
`02-capabilities` has the other half.
