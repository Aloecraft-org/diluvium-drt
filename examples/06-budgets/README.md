# 06-budgets

One program, three apps. A drt app is a config plus a program: `app.dlua` is
the same file in all three runs, and the two configs differ in one number.

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

Abridged where one run repeats the one above it; `expected.txt` is the whole
of it, and the loop is pure arithmetic, so those are the numbers every time.

```
$ drt run app.dlua
   100000 steps   acc = 36212
   200000 steps   acc = 905564
   300000 steps   acc = 742112
   400000 steps   acc = 341711
finished all 400000 steps, acc = 341711
exit 0

$ drt run --config bounded.json
   ... the same five lines, then exit 0

$ drt run --config tight.json
   100000 steps   acc = 36212
drt run: instruction budget of 1000000 exceeded
stack traceback:
	[string "app.dlua"]:20: in main chunk
	[C]: in ?
exit 1
```

It writes nothing to disk.

## What it teaches

**A budget is the deployment's number, not the program's.** `app.dlua` never
asks what it may spend and never checks what is left. It spends, and when the
count runs out the VM stops it where it stands — no callback, no warning at
ninety percent, nothing to negotiate with.

**A bound you fit inside is invisible.** The loop costs 2,400,001 instructions
and `bounded.json` allows ten million, so the first two runs are identical. A
correctly sized budget looks like nothing happening, which is why it is worth
writing down anyway.

**Running out is a reply to the outside, not to the program.** One sentence on
stderr, a traceback naming the line, exit 1 — and the progress line before it
is work that really happened. A supervisor sees a reason, not a hang.

**`instructions` bounds VM instructions and `memory_kb` bounds what the VM
holds. Neither bounds wall-clock time** — a hostcall parked for 85 seconds
spends no instructions — **and neither bounds spawns**: each child carries its
own numbers, not a share of yours. That is why `app.dlua` is a pure loop; it is
the only shape that spends this budget rather than sitting inside it.

**An unstated budget is not a budget of zero.** The first run states none and
finishes all 400,000 steps: absent means unbounded.

**Size budgets for programs you wrote.** A `pcall` around the loop catches
exhaustion as an ordinary error, and enforcement does not resume afterwards. In
v0.4.0, `instructions` is not containment for a program you do not trust.
