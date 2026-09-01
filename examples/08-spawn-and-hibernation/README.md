# 08-spawn-and-hibernation

One program starting another, and a child that parks itself once it has
nothing to do. Children live in the swarm, so this one is `drt start`:
`drt run` is one program and has nowhere to put a second, and `host.spawn`
there waits ten seconds for an answer nobody will give.

## Run it

```
cd examples/08-spawn-and-hibernation
drt start --config app.json
```

Paths in a config resolve against the directory you run from, so start with
the `cd`. `app.json` names `parent.dlua`, the file beside it.

## What you should see

```
$ drt start --config app.json
spawned instance 2
parent: the time is ok for me
  child: the time is denied for me; parking
host.spawn: denied: host:fs/read
event: hibernated 2
```

Exit 0, nothing written to disk, the same five lines every run.

## What it teaches

**A child holds what its parent gives it, and never more.** The parent holds
`lifecycle` and `host:time`; it hands the child `lifecycle` alone, so the same
`host.try('time')` is `ok` in one and `denied` in the other. Neither program
branches on which it is — a refusal is a reply, not an exception.

**Asking for more is refused by name.** The second spawn names `host:fs/read`,
which the parent does not hold, and the refusal says so. Attenuation is checked
before anything is built, so that child never exists. `host.spawn` raises
rather than returning a status, which is why it is the one line in `pcall`.

**A budget is stated, and in v0.4.0 it does not attenuate.** The child's
`instructions = 200000` is enforced on the child — `06-budgets` shows what
running out looks like — but the swarm checks the *names* in a spawn request
and not the *numbers*. A child stating a budget larger than its parent's is
taken at its word today. Do not lean on it against code you did not write.

**A program parks itself, and an ancestor or the deployment may park it too.**
`{op = 'hibernate'}` pushed on `system/lifecycle` is the child asking to be
swapped out. It is snapshotted, stops being resident, and stays alive; its
parent hears `hibernated` the same way it would hear `exited`. The same
request naming a descendant's `id` parks that descendant instead, and a
config with `residency.max_resident` parks the least recently active of the
children spawned with `wake_on_message`. A stranger cannot: parentage is the
only relation the swarm knows.

**Waking is a delivery, and a child has no letterbox yet.**
`wake_on_message = true` means a message pushed at the parked instance restores
it and lands in its queue ahead of anything live. But the messages that arrive
from outside a deployment — a request on a listener — reach the root program,
so a parked *root* wakes on the next one and a parked *child* has nothing that
can address it. A handle is not a channel: there is no `child.push`.
