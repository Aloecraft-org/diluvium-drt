#!/usr/bin/env python3
"""Assert the *deterministic* bench fields against the committed baseline.

    python3 bench/check-fidelity.py bench/c-swarm_bench-baseline.json <fresh-run.json>

This is what belongs in CI. Timings do not: `doc/Benchmarks.md`'s own
doctrine is that a shared runner varies by more than most regressions worth
catching, so a wall-clock assertion is a check that fails when the runner is
busy rather than when something is wrong. What *is* checkable is everything
the design says must reproduce exactly — bytes, counts, and the step counts
the rate limiter produces. A difference in those is a fidelity bug in the
port, not a performance story, and it should break the build.

Allocation counts are checked too, with a ceiling rather than an equality:
they are deterministic for a given build, and the point is to notice when
someone adds an allocation to the hot path — the exact failure that hid a
real result twice on this branch.
"""
import json
import sys

# field -> (case, tolerance). Tolerance 0 means "must match exactly".
EXACT = [
    ("density", "cached_bytes_per_agent"),
    ("density", "resident_bytes_per_agent"),
    ("density", "agents"),
    ("spawn", "small_steps"),
    ("spawn", "rate8_steps"),
    ("spawn", "large_steps"),
    ("queue", "p16_refused_pushes"),
    ("queue", "p256_refused_pushes"),
    ("queue", "p4096_refused_pushes"),
]

# Churn: the same seed drives the C's own xorshift64* and the same LRU
# policy (ties evict the highest index, as the C's <= scan does), so the
# entire cache behaviour reproduces exactly — the die and the policy both.
for _label in ("all_resident", "half_resident", "eighth_resident"):
    EXACT += [
        ("churn", f"{_label}_hit_rate"),
        ("churn", f"{_label}_wakes"),
        ("churn", f"{_label}_hibernates"),
        ("churn", f"{_label}_steps"),
        ("churn", f"{_label}_refused_pushes"),
        ("churn", f"{_label}_wake_buffer_accepted"),
        ("churn", f"{_label}_wake_buffer_refused_of_64"),
        ("churn", f"{_label}_cached_bytes_each"),
    ]

# The guest heap is the C core's own and must match to within rounding.
NEAR = [("density", "resident_bytes_per_agent", 0.001)]

# What DRT's shipped configuration costs over the stock C bench, per field,
# named here rather than absorbed by a band. One entry. DRT opens every
# source instance DV_FLAG_TEXT_ONLY (GUARANTEES.md: source only, because the
# bytecode verifier does not exist), and under that flag the core wraps the
# guest's own `load` so it refuses a precompiled chunk by the second door as
# well as the first -- one C closure holding the real `load` as its upvalue
# (`dlibs.c`, `seal_load_mode`). The C swarm bench opens its workers with
# flags 0 and has no option to do otherwise, so the C's guest heap is DRT's
# minus exactly that closure. Measured on diluvium 0.15.1 (7f952d86): stock
# C 92162, C with its workers text-only 92210, DRT 92210. The number is the
# closure and not a fudge; `doc/0.7.0-ledger.md` has the whole trace.
# Retire it when `swarm_bench` grows a text-only option and the baseline is
# captured in DRT's configuration.
CONFIGURATION = {("density", "resident_bytes_per_agent"): 48.0}


def band(a):
    """How far `got` may sit from baseline `a` and still be the same number.

    Exact means exact. A byte or count figure is an integer in disguise and
    matches to the integer; a ratio the C prints to four decimals matches to
    that printing. The band this replaces was max(1, 0.1%), which at ninety
    thousand bytes is ninety-two of them -- wide enough to have absorbed the
    one named difference above without anyone seeing it, which is how a
    fidelity check stops being one.
    """
    return 5e-5 if abs(a) < 10 else 0.5

# Ceilings: allocations per round trip on the hot path.
CEILING = [
    ("queue", "p16_allocs_per_roundtrip", 5.0),
    ("queue", "p256_allocs_per_roundtrip", 5.0),
    ("queue", "p4096_allocs_per_roundtrip", 5.0),
]


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    with open(sys.argv[1]) as f:
        ref = json.load(f)["cases"]
    with open(sys.argv[2]) as f:
        got = json.load(f)["cases"]

    failures = []
    checked = 0

    for case, field in EXACT:
        if case not in got or field not in got[case]:
            continue
        if case not in ref or field not in ref[case]:
            continue
        a = ref[case][field]
        b = got[case][field] - CONFIGURATION.get((case, field), 0.0)
        checked += 1
        if abs(a - b) > band(a):
            failures.append(f"{case}.{field}: baseline {a}, got {got[case][field]}")

    for case, field, tol in NEAR:
        if case in got and field in got[case] and case in ref and field in ref[case]:
            a = ref[case][field]
            b = got[case][field] - CONFIGURATION.get((case, field), 0.0)
            checked += 1
            if a and abs(a - b) / abs(a) > tol:
                failures.append(
                    f"{case}.{field}: baseline {a}, got {got[case][field]} (past {tol:.1%})"
                )

    for case, field, ceiling in CEILING:
        if case in got and field in got[case]:
            v = got[case][field]
            checked += 1
            if v > ceiling:
                failures.append(
                    f"{case}.{field}: {v:.2f} allocations per round trip, past the "
                    f"{ceiling} ceiling — something was added to the hot path"
                )

    if failures:
        print(f"fidelity: {len(failures)} of {checked} checks FAILED\n")
        for f in failures:
            print(f"  {f}")
        print(
            "\nThese fields are deterministic by design. A difference is a "
            "fidelity bug in the port, not a slow runner."
        )
        return 1
    named = ", ".join(
        f"{c}.{f} net of {v:g} B ({'text-only load wrapper' if f == 'resident_bytes_per_agent' else 'named'})"
        for (c, f), v in CONFIGURATION.items()
    )
    print(f"fidelity: {checked} deterministic checks pass" + (f" ({named})" if named else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
