#!/usr/bin/env python3
"""Diff a drt-bench run against the captured C swarm_bench baseline.

    cargo run --release -p drt-bench -- --json --seed 7 > /tmp/drt.json
    python3 bench/compare.py bench/c-swarm_bench-baseline.json /tmp/drt.json

Ratios are DRT / C, so below 1.00 is DRT ahead on a cost and behind on a
throughput — the field name says which. Deterministic rows (bytes, counts,
steps) must read 1.00; anything else there is a fidelity bug in the port,
not a performance story.
"""
import json
import sys


def load(path):
    with open(path) as f:
        return json.load(f)["cases"]


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 1
    c, d = load(sys.argv[1]), load(sys.argv[2])
    print(f"{'field':<44}{'C (dvs.c)':>16}{'DRT (rust)':>16}{'ratio':>9}")
    print("-" * 85)
    for case in sorted(d):
        print(f"== {case}")
        for k, v in sorted(d[case].items()):
            cv = c.get(case, {}).get(k)
            if cv is None:
                print(f"  {k:<42}{'—':>16}{v:>16.4g}{'new':>9}")
            elif cv == 0 and v == 0:
                print(f"  {k:<42}{cv:>16.4g}{v:>16.4g}{'1.00':>9}")
            elif cv == 0:
                print(f"  {k:<42}{cv:>16.4g}{v:>16.4g}{'—':>9}")
            else:
                print(f"  {k:<42}{cv:>16.4g}{v:>16.4g}{v / cv:>9.2f}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BrokenPipeError:
        # Piped into `head` and friends: not an error worth a traceback.
        sys.exit(0)
