# Log

Append only. One entry per run: plan, what was done, what was learned,
what is next.

## 2026-10-03 multi-name --park

Plan: the owner's queued work order, item 4 (one answerer for several
names), asked for in the project thread. Roadmap I0 (seed) is still
active and unfilled, so this run sits outside it at the owner's request.
One branch, `claude/project-thread-qxlh4a`, one pull request.

Done: `--park` repeats and `p2p.park` takes a list; one host behind every
name, one presence (poll and stream) per name; a refused name stops alone.
Session keys carry the name when there are several, because the
reference server numbers call ids per name (`c1` at each). Verified by a
test that parks two names and calls both at once, and fails without the
keying.

Learned: `pkill -f` on a pattern that matches the shell's own command
line kills the shell; use the job's pid.

Next: item 5, the TURN design note.
