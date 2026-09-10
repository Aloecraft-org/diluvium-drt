# 27-fft

The transform whose entire claim is that its bits are identical on every
target. This is where a twiddle factor that drifted on one of them becomes
visible.

Needs a `drt` whose embedded core was built with `numeric` (`drt buildinfo`
lists it under `features`).

## Run it

```
cd examples/27-fft
drt run app.dlua
```

## What you should see

```
8 real samples -> array<c128>[5]

the spectrum
  real  4042000000000000 c010000000000000 c010000000000000 c010000000000000 c010000000000000
  imag  0000000000000000 4023504f333f9de6 4010000000000000 3ffa827999fcef30 0000000000000000
  size  4042000000000000 4024e7ae9144f0fc 4016a09e667f3bcd 4011517a7bdb3894 4010000000000000

the round trip
  in    3ff0000000000000 4000000000000000 4008000000000000 4010000000000000 4014000000000000 4018000000000000 401c000000000000 4020000000000000
  out   3ff0000000000000 4000000000000000 4008000000000000 4010000000000000 4014000000000000 4018000000000000 401c000000000000 4020000000000000
```

It writes nothing to disk.

## What it teaches

**`rfft` is the transform for a real signal.** The spectrum of a real input is
conjugate-symmetric, so the second half carries nothing the first half does not
already imply. `rfft` returns the `n/2+1` bins that are not redundant — five,
for eight samples — instead of eight bins where three are mirrors.

**Complex is a dtype, not a pair of arrays.** The result is `c128`, and
`array.bits` prints each element as `real:imag`. Here it is split with
`array.real` and `array.imag` to keep the lines readable; `array.magnitude`
gives the third line, which is the one most callers actually want.

Bin 0 is `4042000000000000` — 36.0, the sum of 1 through 8, with a zero
imaginary part. That is the DC term, and it is the one value in the output you
can check by hand, which makes it the first thing to look at when a target
disagrees.

**The round trip is exact here, and that is luck of the input, not a promise.**
A transform costs rounding in both directions; some signals come back a bit or
two light. The check is on bit patterns precisely so that when it happens it is
*visible* — a differing hex digit in a diff — instead of being rounded away by
`%g` into output that looks identical while the numbers are not.

**This is the example that catches a compiler flag.** The kernels are compiled
with `-ffp-contract=off` on every target; without it a compiler is free to fuse
a multiply and an add into one instruction that rounds once instead of twice.
That is a better answer, and a different one. A build path that missed the flag
produces a spectrum that differs in the last digit or two on that target alone —
which looks like nothing at all until this diff shows it.
