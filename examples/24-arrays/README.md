# 24-arrays

The `array` library: a typed buffer the core owns, and the printing rule that
makes one expected output valid on every target.

Needs a `drt` whose embedded core was built with `numeric`. `drt buildinfo`
lists it under `features`; without it `array` is nil and the first line raises.

## Run it

```
cd examples/24-arrays
drt run app.dlua
```

## What you should see

```
what they are
  ints    array<i64>[4]   dtype i64
  floats  array<f64>[4]   dtype f64

what is in them
  ints    0000000000000001 0000000000000002 0000000000000003 0000000000000004
  floats  3ff0000000000000 4004000000000000 c00e000000000000 3fb999999999999a

elementwise
  ints + ints  0000000000000002 0000000000000004 0000000000000006 0000000000000008
  floats * 2   4000000000000000 4014000000000000 c01e000000000000 3fc999999999999a

masks
  positive  01 01 00 01
  selected  3ff0000000000000 4004000000000000 3fb999999999999a

two dimensions
3ff0000000000000 4000000000000000
4008000000000000 4010000000000000
```

Those are the numbers every time, on every target. It writes nothing to disk.

## What it teaches

**An array is not a Lua table.** `array.from` copies the values into a buffer
the core owns, with one element type for the whole thing. `tostring` names the
shape — `array<f64>[4]` — because printing the contents is a separate decision
with a separate answer, below.

**The dtype follows the values, and integer stays integer.** A table of
integers is `i64`; one float anywhere makes the whole array `f64`. Arithmetic
keeps the type, so `ints + ints` is exact integer addition, not a float result
that happens to look right. `/` and `^` are float operators, exactly as in Lua.

**Never print a float with `%g`.** `0.1` is `3fb999999999999a` here and on
every other target, but glibc, musl, wasmtime, Chromium and mingw do not all
turn that pattern back into the same decimal text. An example that printed
decimals would pass on the machine it was written on and fail elsewhere for a
reason that is not a bug. `array.bits` prints the IEEE pattern instead: 16 hex
digits per `f64`, 2 per `u8`, a row per line for 2D. That is the whole reason
one `expected.txt` can gate this example on four targets at once.

**A comparison answers a mask, not booleans.** `array.gt` returns a `u8` array
of 0s and 1s — printed here as `01 01 00 01` — and that mask is what
`array.select` compacts with. Keeping it an array is what lets the comparison
and the selection both stay inside the core.
