# 26-reductions-and-grouping

Three rules decide what a reduction answers. All three exist so that the answer
is *the same* on every target, rather than merely close on each of them.

Needs a `drt` whose embedded core was built with `numeric` (`drt buildinfo`
lists it under `features`).

## Run it

```
cd examples/26-reductions-and-grouping
drt run app.dlua
```

## What you should see

```
a sum that depends on the order it is taken in
  left to right  4341c37937e08000
  array.sum      4341c37937e08003

NaN sorts last, so it loses min and wins max
  sorted  3ff0000000000000 4000000000000000 fff8000000000000
  min     3ff0000000000000
  max     fff8000000000000

grouping, by first appearance: 3 groups
  ids         0000000000000001 0000000000000002 0000000000000001 0000000000000003 0000000000000002
  group_sum   4014000000000000 4032000000000000 4020000000000000
  group_mean  4004000000000000 4022000000000000 4020000000000000
```

It writes nothing to disk.

## What it teaches

**Rule 1: the summation order is canonical, and it is not left to right.** The
input is `1e16` followed by eight `1.0`s. Added left to right, each `1.0` is
smaller than the gap between representable doubles up there, so every one of
them rounds away and the answer is exactly `1e16` — `4341c37937e08000`.
`array.sum` splits the input across eight accumulators and combines them
pairwise, so six of the ones survive: `4341c37937e08003`, three ulps up.

Neither answer is wrong. The point is that the second one is *fixed*: it does
not change if the core vectorises the loop, if a target has wider registers, or
if the array is chunked differently tomorrow. A left-to-right sum is only
reproducible for as long as nobody optimises it.

**Rule 2: one total ordering, with NaN last.** `sort` puts NaN at the end, and
`min` and `max` are defined as the first and last of that order — so a NaN
never wins `min` and always wins `max`. It would be friendlier to have `max`
skip NaN, and that is exactly what makes it wrong: `max` would then disagree
with `sort` about which element is the largest. One rule, applied everywhere,
beats two conveniences that contradict each other.

**Rule 3: group ids are handed out in first-appearance order.** `group_index`
numbers `{10, 20, 10, 30, 20}` as `1, 2, 1, 3, 2` — first seen is 1, next new
key is 2. Not by hash (a seed would leak into the answer) and not by sorted key
order (that would make the ids depend on a comparison the caller never asked
for). Scanning once and numbering on first sight is the only rule that needs no
extra input, which is why the ids are reproducible.

**Reductions answer Lua numbers, so they need wrapping to print.** `array.sum`
hands back a plain number, and printing one with `%g` is the thing these
examples never do — hence the three-line `bits` helper at the top. It is the
same `array.bits`, given a one-element array.
