# 12-under-the-hood

`host.time()` is not a function the runtime provides. It is a message pushed
onto a queue and an answer popped off another one — and this is that, written
out. One `.dlua` file and no config, which is a whole app.

## Run it

```
cd examples/12-under-the-hood
drt run app.dlua
```

## What you should see

```
by hand, on the raw queue pair:
  time                   ok      1788290538171
  sql/query              denied  no connector is wired for 'sql/query' in this process

the same two calls, through the library that owns that same pair:
  host.time()            ok      1788290538171
  host.try("sql/query")  denied  no connector is wired for 'sql/query' in this process
```

The clock readings are from the run that made this text; yours will differ.
Both halves report the same status and the same sentence, because they are the
same two messages.

## What it teaches

**A hostcall is a message, not a call.** A request names three fields — `tok`,
`call`, and `args` when the call takes any — and a reply names four and carries
three: `tok`, `status`, and `value` when the status is `ok` or `detail` when it
is not. Nothing else is reserved, so a call that needs more invents fields
inside `args`. That shape is why the whole conversation is in the message log
already, and why a replay replays a recorded moment instead of asking the
clock again.

**A refusal is a reply, not an exception**, and here is the reason. `denied` is
not a thrown thing that `host.try` catches. It is an ordinary table that
arrived on `host/replies` like every other. `host.call` reads `status` and
raises; `host.try` reads it and hands it back.

**The library is a surface, not a privilege.** `host.time()` pushed onto the
queues this program declared — there is one `host/calls` per instance, and the
library looks it up before declaring its own — and it reached the same
dispatcher, through the same grant check, as the ten lines above it. Going
underneath gains nothing and costs three things: the reply timeout, the stash
for replies that arrive out of order, and a token space starting at 2^30 that
a hand-rolled one cannot collide with.

So write the short one. Read this one because it generalises: a call the
library has no wrapper for is `host.call("fs/list", {path = "."})` — these ten
lines, with the ceremony back inside the library where it belongs.
`doc/Hostcall.md` is the encoding in full.
