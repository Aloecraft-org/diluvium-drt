# 03-writing-dlua

The language, for someone who already writes Lua. One `.dlua` file and no
config, which is a whole app.

## Run it

```
cd examples/03-writing-dlua
drt run app.dlua
```

## What you should see

Each addition run rather than described. Abridged; `expected.txt` is the whole
of it, and it is the same on every run.

```
what drt seals
  type(require)                  nil
  type(os)                       nil

interpolation, and ?? for nil
  $"{ratio::%.2f}"               0.67
  false ?? 8080                  false
  false or 8080                  8080

the clock is not in `time`
  time.now()                     raises: ... attempt to call a nil value (field 'now')
  host.time()                    a number, milliseconds
```

## What it teaches

**Backward compatible is a claim about the syntax, not about the library.** A
`.dlua` file containing nothing but Lua parses and runs, so the additions are
there where they help and nowhere else. But `os`, `io` and `package` — and so
`require` — are ambient authority, a reach the config does not describe. They
are sealed, with no flag to unseal, and what they did goes through `host.*`
instead, where it costs a grant and a connector.

**There is no `time.now()`, and the error does not say so.** `time` is loaded,
so you find it, and then it fails like a gap in the library. It is not a gap.
`time` is the pure calendar — arithmetic over a moment you already hold,
needing no grant and no connector, and answering the same in every replay. The
clock is `host.time()`, because a clock is a capability: it costs a grant, a
connector answers it, and that is what lets a replay replay the recorded
moment instead of the replayer's.

**The other half of that trap is units.** `host.time()` answers in
milliseconds; every `time` function speaks seconds, the unit `os.time` and a
JWT `exp` already use. Divide at the boundary —
`time.iso(host.time() // 1000)` — or you get the year 58583, with no error.
