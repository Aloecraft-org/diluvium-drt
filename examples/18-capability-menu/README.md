# 18-capability-menu

What can this instance reach? One call answers it, and the answer is the
deployment's, not the program's guess.

## Run it

```
cd examples/18-capability-menu
drt run --config auditor.json
drt run --config worker.json
drt run --config ungranted.json
```

The same `app.dlua` runs under all three. Only the config changes.

## What you should see

```
$ drt run --config auditor.json
  crypto       builtin  granted=false
  fs           builtin  granted=false
  time         builtin  granted=false
  capabilities builtin  granted=true

$ drt run --config worker.json
  crypto       builtin  granted=true
  fs           builtin  granted=true
  time         builtin  granted=false
  capabilities builtin  granted=true

$ drt run --config ungranted.json
drt run: …: host.capabilities.list: denied: 'capabilities/list' is outside this instance's grants
```

## What it teaches

**Two different questions, answered in one row.** The rows are what the
process *wired*; `granted` is what this *instance* may reach. All three
configs wire the same three connectors, and the column moves anyway. A
program that only tried calls could not tell "nothing is wired for time" from
"time is wired and not mine".

**An auditor holds the question, not the answers.** `auditor.json` grants
`host:capabilities/list` and nothing else, and reports the deployment's whole
reach with every `granted` false. That is the shape this call exists for: the
thing that inventories a fleet should not be the thing with the most access in
it.

**A family held narrowly still reads as reached.** `worker.json` grants
`host:fs/read` — one verb of four — and `fs` reports `granted=true`. The row
answers "can this instance get into this family at all", which is the question
worth asking before a call, and the call itself is what answers the rest.

**The menu is gated like everything else.** `ungranted.json` wires the
same three and does not grant the question, so asking is refused rather than
answered with a quiet empty list. An empty list would be indistinguishable
from a deployment that wired nothing, which is exactly the wrong thing to be
unable to tell apart.

**`kind`, `owner` and `visibility` are the C host's fields, kept.** Every
family here is `builtin` with a nil `owner` and `public` visibility, because
DRT has no plugins to own a family and no visibility policy yet. They are on
the wire so a program written against `dhost.c`'s `conn_capabilities` reads
the same map from either host.

`02-capabilities` is the other half of this: what a refusal looks like when
you make the call instead of asking first.
