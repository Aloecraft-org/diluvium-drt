# 07-sql

A SQLite database from a program. `sql/exec` writes, `sql/query` reads, and
the config decides which of the two exists. One program, two apps:
`readwrite.json` and `read-only.json` differ in a single word.

## Run it

```
cd examples/07-sql
drt run --config readwrite.json
drt run --config read-only.json
```

In that order. Paths inside a config resolve against the directory you run
from, so start with the `cd`. Both configs grant `.`, the directory these
files sit in, and the first run creates `notes.db` there.

## What you should see

Abridged, because the second run repeats most of the first. `expected.txt` is
the whole of it, and it is the same text every run.

```
$ drt run --config readwrite.json
sql/exec, one writing statement at a time:
  drop table if exists notes                   ok      0 changed, rowid 0
  create table notes (body text)               ok      0 changed, rowid 0
  insert into notes values (?), (?)            ok      2 changed, rowid 2

sql/query, and ? binds on this side too:
  select rowid, body from notes order by rowid ok      1 first note | 2 second note
  select body from notes where rowid = ?       ok      second note

what the connector refuses, and in whose words:
  delete from notes                            error   'sql/query' is for statements that read; this one writes, which is 'sql/exec' and a different grant
  open 'sub/notes.db'                          error   'sub/notes.db' contains a path separator; name a database inside the granted scope, not a path to one

$ drt run --config read-only.json
sql/exec, one writing statement at a time:
  drop table if exists notes                   error   'sql/exec' needs access = "readwrite"; this scope is read-only
  create table notes (body text)               error   'sql/exec' needs access = "readwrite"; this scope is read-only
  insert into notes values (?), (?)            error   'sql/exec' needs access = "readwrite"; this scope is read-only

  ...and then the same two queries and the same two refusals as above.
```

Both runs exit 0. The rows the second run reads are the ones the first run
wrote: `notes.db` stays on disk between them, and is in this directory's
`.gitignore`.

## What it teaches

**The scope is a directory; the database is a name in it.** A config naming
one file would make "which database" a deployment question, and it is an
application question — so the config grants `.` and the program says
`host.sql.open("notes.db")`. A name and never a path: `sub/notes.db` is
refused for the separator, which is stricter than an `fs` scope, where a path
may descend. `create` follows the write grant, so the readwrite app makes
`notes.db` on first use and the read-only one would not.

**`access` decides whether `sql/exec` exists.** `readwrite` wires both calls;
`read` wires `sql/query` alone and refuses every write by name, before the
statement is prepared. It defaults to `read`. Nothing else in the two configs
differs, so every line that changes in the second run is that one word.

**The split is what a statement does, not what it is called.** `delete from
notes` asked of `sql/query` comes back `error` — because SQLite prepared it
and reported it is not read-only, not because a regex found the word. There
is no phrasing that gets a write past the query grant.

**One statement per call.** A string holding two is refused at prepare, which
is why the schema and its rows here are three `sql/exec` calls rather than
one. And the database outlives the process, so this program drops its table
before creating it; that is what makes a second run print what the first did.

**A result too large is refused, not truncated.** `max_result_rows` caps one
`sql/query` and defaults to 1000. Past it the call answers an error saying to
page with `LIMIT`/`OFFSET`, because a truncated answer would be a silent lie.
