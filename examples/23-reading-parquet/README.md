# 23 — reading parquet

```
drt run --config app.json
```

## What it teaches

**A column is bytes, not a table.** `data/read_parquet` answers with the
column's raw little-endian bytes, so a million-row `f64` column is eight
megabytes in a Lua string rather than a million Lua values. A build with
the `numeric` feature adopts those bytes into an `array` with no copy at
all; a build without it — which is every build today — reads them as a
string, and `string.unpack` is how you get at one.

**Text is a dictionary.** A text column answers as `i64` codes plus
`uniques`, each distinct string stored once, for the same reason: a
million-row city column is a million integers and two strings, not a
million strings.

**Bits, never `%g`.** The output prints IEEE bit patterns because one
`expected.txt` has to be right on glibc, musl, wasmtime and Windows, whose
C libraries format decimals differently from each other. `3fb999999999999a`
is 0.1 on every one of them; `0.1` is four different strings.

## What it needs

A `full` build: `data` is `full`-only, because the parquet reader is a
large dependency and the smaller profiles exist to not carry things like
it. On a build without it, `drt` refuses at startup by name — "config
wires connector 'data', which this build does not carry" — rather than
failing at the first call.

The connector is wired to a **place**, the same discipline `fs` and `sql`
follow: `app.json` grants the directory, the program names the file inside
it, and `access = "readwrite"` is what makes the two writing verbs answer
rather than refuse. It is literally `fs`'s jail, so a path resolving out of
the granted directory is refused here exactly as it is there, symlinks
followed.

## Nulls

Not shown above, because a four-row example that also demonstrates nulls
teaches neither well. The rule, from the numeric spec's Stage 4: an `f64`
column says null with NaN and carries no mask; every other column carries
an optional `u8` `valid` beside it, `1` where the row has a value. A column
with no null row carries no mask at all.
