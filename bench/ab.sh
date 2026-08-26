#!/bin/sh
# Interleaved A/B capture: the only honest way to compare times on a shared
# runner.
#
# Capturing one side and then the other — even minutes apart — compares two
# machine states, not two implementations. This branch quoted ratios built
# that way for most of a day; `tight_table_us_per_step` read 184 us from a
# C baseline captured in the morning and 67-91 us from the same binary that
# afternoon, so every ratio against it was measuring the container's mood.
#
# This alternates C, DRT, C, DRT... N times and medians each side, so both
# see the same interference.
#
#   sh bench/ab.sh 5 /path/to/diluvium/dist/swarm_bench
set -eu
N="${1:-5}"
CBIN="${2:-/home/user/aloecraft-org/diluvium/dist/swarm_bench}"
OUT="${TMPDIR:-/tmp}/drt-ab"
mkdir -p "$OUT"
rm -f "$OUT"/c-*.json "$OUT"/d-*.json

cargo build --release -p drt-bench >/dev/null 2>&1
DBIN="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/release/drt-bench"

i=1
while [ "$i" -le "$N" ]; do
  "$CBIN"  --json --seed 7 > "$OUT/c-$i.json" 2>/dev/null
  "$DBIN"  --json --seed 7 > "$OUT/d-$i.json" 2>/dev/null
  printf 'pass %s of %s\n' "$i" "$N" >&2
  i=$((i + 1))
done

python3 - "$OUT" <<'PY'
import glob, json, statistics, sys
out = sys.argv[1]

def median_runs(pattern):
    runs = [json.load(open(f)) for f in sorted(glob.glob(pattern))]
    cases = {}
    for case in runs[0]["cases"]:
        merged = {}
        for k in runs[0]["cases"][case]:
            vals = [r["cases"][case][k] for r in runs if k in r["cases"].get(case, {})]
            if vals:
                merged[k] = statistics.median(vals)
        cases[case] = merged
    return cases

c = median_runs(f"{out}/c-*.json")
d = median_runs(f"{out}/d-*.json")
json.dump({"tool": "swarm_bench", "interleaved": True, "cases": c},
          open("bench/c-swarm_bench-baseline.json", "w"), indent=2)
json.dump({"tool": "drt_bench", "interleaved": True, "cases": d},
          open("bench/drt-bench-run.json", "w"), indent=2)
print("wrote bench/c-swarm_bench-baseline.json and bench/drt-bench-run.json")
PY
