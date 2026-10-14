# Log

Append only. One entry per run: plan, what was done, what was learned,
what is next.

## 2026-10-03 batch: a 0.8 candidate for browser access

Plan: one item came in, a request for a 0.8 release candidate that
carries the browser-access host. It was received as a request, not as
an approval. The current iteration is I0 Seed, which says
Unattended: no. Publishing a release is not in the autonomous tier,
so it is approve_first. Steps: (1) prepare the cut as a pull request,
following doc/Handoff.md's release track step 2, on
claude/project-thread-k93f54; (2) find a dev build whose browser and
browser-access jobs passed, and check its install line in a clean
environment; (3) queue the publish for the owner. This entry and the
queue item go in a pull request (claude/project-thread-k93f54-1), not
self-merged. Any push to main starts a dev build, which would prune
v0.8.0-dev.19, the build downstream pins. Even with [skip ci], the
nightly would cut once main moved.

Done:
- #46: release: cut the v0.8.0-rc.1 entry. changelog validate,
  generate, check, consistency and release-check --publish all pass,
  and so does the_hard_coded_core_facts. It waits for the owner, since
  owner-paths covers .technoproj.
- v0.8.0-dev.27 (a4b3fe9, main): browser, browser-access-client,
  ssh-page, test and every features job passed in CI. Its install.sh,
  run under env -i with a fresh HOME, printed `checked: sha256 ok` from
  GitHub. The mirror was unreachable from this session.
- Q-001 queued: approve and publish v0.8.0-rc.1.
- No tag cut and no workflow dispatched. Both are approve_first.

Learned: dev.23 and later are Linux-only fast-path builds (12 assets).
The rc builds every target, as dev.22 last did.
doc/Handoff.md still says the 0.8.0 branch is unmerged. It merged in
#40, so step 1 of its release track is done.

Next: once the owner merges #46 and the rc is tagged, check the landing
and the rc's install line on a clean machine.

## 2026-10-14 run: skip a `#!` first line in program files

Plan: the owner asked, in the project thread, for drt to ignore a `#!`
line at the top of a program file the way the `diluvium` executable
already does. One step on claude/project-thread-f02eo0: skip it where a
file becomes source (`modules::program_load` and the module walk),
test it, open a pull request. No changelog entry: the newest entry is
the released 0.8.0-rc.1, and opening the next one means choosing the
next version and moving every stamp, which is the owner's call.

Done:
- `modules::skip_first_line_comment`, applied to the entry and to each
  module as read. Keeps the newline, so line numbers hold.
- Unit tests in `modules` (18 passed); fmt and clippy clean for
  `-p drt --features full`.
- End to end: a file starting `#!/usr/bin/env -S drt run -f`, run as
  `./tool`, printed and reported its error at line 3.

Learned: `drt run` takes no trailing arguments, so `./tool a b` is
refused before the program loads. A bare `#!/usr/bin/env drt` is
refused too (`unrecognized subcommand`). Both are separate asks.

Next: the owner decides the next changelog version, and whether drt
should accept a bare path and pass arguments through.
