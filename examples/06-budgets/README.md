# 06-budgets

What a budget bounds, what it does not, and what running out of one looks
like from outside the process.

One program, three drt apps. An app is a config plus a program: `app.dlua`
is the same file in all three runs, and `bounded.json` and `tight.json`
differ in exactly one number.

## Run it

```
cd examples/06-budgets
drt run app.dlua
drt run --config bounded.json
drt run --config tight.json
```

Paths in a config resolve against the directory you run from, so start with
the `cd`. Both configs name `app.dlua`, the file beside them.

## What you should see

```
$ drt run app.dlua
   100000 steps   acc = 36212
   200000 steps   acc = 905564
   300000 steps   acc = 742112
   400000 steps   acc = 341711
finished all 400000 steps, acc = 341711
exit 0

$ drt run --config bounded.json
   100000 steps   acc = 36212
   200000 steps   acc = 905564
   300000 steps   acc = 742112
   400000 steps   acc = 341711
finished all 400000 steps, acc = 341711
exit 0

$ drt run --config tight.json
   100000 steps   acc = 36212
drt run: instruction budget of 1000000 exceeded
stack traceback:
	[string "app.dlua"]:27: in main chunk
	[C]: in ?
exit 1
```

The program is pure arithmetic, so this is what every run prints: the same
numbers, and the run that exceeds its bound stopping in the same place. It
writes nothing to disk.

## What it teaches

**A budget is the deployment's number, not the program's.** `app.dlua` never
asks what it is allowed to spend and never checks how much is left. It
spends, and when the count the config named runs out the VM stops it where it
stands. There is no callback, no warning at ninety percent, and nothing for
the program to negotiate with.

**A bound you fit inside is invisible.** The first two runs print the same
five lines. The whole loop costs 2,400,001 instructions — binary search on
the number pins it exactly: 2,400,000 fails and 2,400,001 finishes — and
`bounded.json` allows ten million, so the only evidence of the ceiling is
that nothing happened. That is what a correctly sized budget looks like, and
it is why the number is worth writing down in the runs where it never bites.

**Running out is not something the program can smooth over.** From outside
the process it is one sentence on stderr, a traceback naming the line the
count ran out on, and exit 1. Everything printed before that is still on
stdout: the first progress line is work that really happened. A supervisor
sees a non-zero exit and a reason, rather than having to guess whether the
program hung.

**Both fields bound the guest VM, and nothing else.** `instructions` is VM
instructions executed; `memory_kb` is how much the VM may hold. Exceeding
`memory_kb` also stops the run and exits 1, but the shape is not identical:
it is `drt run: not enough memory` on stderr with no traceback at all, so
there is no line number to point at. The negative space is the part worth
stating, because it is not what most people assume:

- **Neither bounds wall-clock time.** A hostcall that parks for 85 seconds
  consumes no instructions, so a program waiting on a slow `rest/get` or a
  long `ssh/exec` sits under an instruction budget indefinitely. v0.4.0 has
  no field for a time limit.
- **Neither bounds spawns.** The numbers are not cumulative over a lineage.
  A program that spawns a thousand children is not held to its own budget by
  doing so; each child carries its own.

That is why `app.dlua` is a pure loop with no hostcalls in it. A loop is the
only shape that spends this budget rather than sitting inside it.

**An unstated bound is not a bound of zero.** The first run states no budget
at all and finishes all 400,000 steps: an absent `budget` installs no
counting hook, so the run is unbounded, not bounded at nothing. That is the
root's case, and it is the one this example shows.

**Attenuation is the design's rule, and v0.4.0 does not enforce it on
budgets.** The same block a config carries is what a spawn request carries
one level down, and the intent is that a child states a smaller number and
never a larger one. `drt-config` has the check — `Budget::fits_within`, with
unit tests — but nothing on the spawn path calls it: `Swarm::do_spawn` takes
the request's `budget` verbatim, and an absent one leaves the child
unbounded rather than inheriting the parent's ceiling. Capability grants do
attenuate at spawn; budgets do not yet. `drt run` runs a single instance, so
there is no child here either way.

**In v0.4.0 the instruction budget is escapable from inside the guest.** A
`pcall` around the loop catches the exhaustion as an ordinary Lua error, and
enforcement does not resume afterwards: a program that catches it once runs
unbounded from then on, including outside any `pcall`. Size budgets for
programs you wrote and want to keep honest. Do not lean on `instructions` as
containment for a program you do not trust.
