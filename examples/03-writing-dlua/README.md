# 03-writing-dlua

The language, for someone who already writes Lua. One `.dlua` file, no
config, nothing to install past the `drt` binary.

Diluvium is Lua 5.5 plus a short list of additions, and the syntax is
backward compatible: a `.dlua` file containing nothing but Lua parses and
runs, so none of this is something you have to adopt — it is there where
it helps.

The library surface is the other half of that question, and the program
answers it first, because it is the half that decides whether the Lua you
already have runs here. `drt run` seals the standard library: no `os`, no
`io`, no `package`, and so no `require`.

## Run it

```
cd examples/03-writing-dlua
drt run app.dlua
```

## What you should see

The seal, six additions, and then the trap — each one run rather than
described. Abridged here; `expected.txt` is the whole of it:

```
backward compatible is about the syntax
  type(require)                       nil
  type(os)                            nil
  ("still Lua"):upper()               STILL LUA

string interpolation
  $"hi {user}"                        hi ana
  $"{ratio::%.2f}"                    0.67

null coalescing
  false ?? 8080                       false
  false or 8080   (for contrast)      8080

safe navigation
  cfg?.logging?.level ?? "info"       info
  handler?.on_event(mark())           not called; mark() ran: false

switch
  switch 302 -> case 301, 302         redirected

defer
  work() with two defers              returned
                                      working
                                      closed
                                      released (registered first, runs last)

with
  with a = ..., b = ... do            inside the block
                                      closed b
                                      closed a

the clock is not in `time`
  time.now()                          raises: ... attempt to call a nil value (field 'now')
  host.time()                         a number, milliseconds since the epoch
```

The output is the same on every run: the program prints no timestamp, and
the one moment it formats is a literal.

## What it teaches

**Backward compatible is a claim about the syntax, not about the
library.** `os`, `io` and `package` — and so `require` — are ambient
authority: a program holding them has a reach the config does not
describe, and a run that shells out cannot be replayed. `drt run` seals
them, with no flag to unseal, and what they did goes through `host.*`
instead, where it costs a grant and a connector. So plain Lua syntax
pastes in fine; plain Lua that requires a module or opens a file does
not. This example is one file for that reason, not for tidiness.

**The additions are small and each replaces a Lua idiom that loses
something.** `??` tests for nil where `or` tests for falsiness, so
`false ?? 8080` keeps the `false` you were handed and `false or 8080`
throws it away. `?.` skips the rest of the chain — including the call and
its arguments, which the program proves by checking that the argument's
side effect never happened. `switch` evaluates its subject once and does
not fall through. `defer` runs on every way out of a block, a raise
included. `with` is `defer` for values that own their own closing, since
it binds a to-be-closed local and needs a `__close` metamethod.

**There is no `time.now()`, and the error does not tell you that.** This is
the hour everyone spends. `time` is a library and it is loaded, so you find
it, and then `time.now()` fails with

```
attempt to call a nil value (field 'now')
```

which reads as a gap in the library you just found rather than as a
signpost to a different one. It is not a gap. `time` is the *pure calendar*
— `time.iso`, `time.parse`, `time.fields`, `time.of` — arithmetic over a
moment you already hold, needing no grant and no connector and giving the
same answer on every machine and in every replay.

The clock is `host.time()`, because a clock is a capability. It costs a
grant, a connector answers it, the answer arrives as a message, and that is
what lets a replay replay the recorded moment instead of the replayer's.

**The other half of that trap is units.** `host.time()` answers in
milliseconds; every `time` function speaks seconds, the unit stock Lua's
`os.time` and a JWT `exp` already use. Divide at the boundary —
`time.iso(host.time() // 1000)`. The program shows what you get when you do
not: a date in the year 58583, with no error.

## Two things this build does not have

Verified while writing this example, so you do not have to find them:

- **`?:` does not exist.** There is no safe method-call operator; `h?:m()`
  is a syntax error. Safe navigation is `?.` and `?[` only.
- **A `switch` subject cannot start with `(`.** `switch (x) do` is a syntax
  error, because `switch (x)` is a function call in stock Lua and has to
  stay one. Write `switch x do`, the same shape as `if x then`. This is the
  cost of `switch` being a contextual keyword, which is what lets code that
  already uses `switch` as a name keep working.
