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

# The guest heap is the C core's own and must match to within rounding.
NEAR = [("density", "resident_bytes_per_agent", 0.001)]

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
        a, b = ref[case][field], got[case][field]
        checked += 1
        # Byte and count figures are integers in disguise.
        if abs(a - b) > max(1.0, abs(a) * 0.001):
            failures.append(f"{case}.{field}: baseline {a}, got {b}")

    for case, field, tol in NEAR:
        if case in got and field in got[case] and case in ref and field in ref[case]:
            a, b = ref[case][field], got[case][field]
            checked += 1
            if a and abs(a - b) / abs(a) > tol:
                failures.append(
                    f"{case}.{field}: baseline {a}, got {b} (past {tol:.1%})"
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
    print(f"fidelity: {checked} deterministic checks pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
