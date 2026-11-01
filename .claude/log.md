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

## 2026-10-27 run: `:clear` and a shorter help hint in the REPL

Plan: the owner asked, in the project thread, for exit and clear-screen
commands in both REPLs and a banner that says `Show help: <:h+enter>`.
One step on claude/project-thread-ej00ig: add `:clear` beside the
existing `:quit`, change the banner, fix every place that quotes it,
test, open a pull request. The diluvium half is its own pull request.
No changelog entry, for the reason the 2026-10-14 entry gives.

Done:
- `:clear` in `meta`, listed in `META_HELP`; writes Ctrl+L's bytes,
  only when stdout is a terminal.
- Banner changed in `Repl::banner` and in every test, doc and expected
  file that quoted it.
- `tests/repl.rs` 15 passed, `drt-web` `term` 7 passed; fmt and clippy
  clean for `-p drt --features full`. In a pty under a terminal
  emulator, `:clear` left only the prompt and `:q` exited 0.

Learned: the owner tried this on 0.7.0, which predates the colon
commands, so `:q` was evaluated as Lua there. Ctrl+L leaving the line
off screen did not reproduce in two emulators; the bytes are the ones
bash writes.

Next: colon commands exist only in the native REPL. The page and the
SSH-served REPL print the same banner but evaluate `:h` as Lua.

## 2026-10-27 run: Ctrl+L and `:clear` hid the prompt in VS Code

Plan: the owner showed Ctrl+L in VS Code's terminal clearing the screen
and leaving the redrawn line one row above the viewport; Terminator was
fine. Change the clear from `ESC[H ESC[2J` to `ESC[H ESC[J` in ego-cli
(Ctrl+L), diluvium's dline.c, and `:clear` here, then bump the ego-cli
pin once its pull request merges.

Done: `CLEAR_SCREEN` changed; in a pty `:clear` left only the prompt.
Learned: in @xterm/headless with scrollOnEraseInDisplay on, ED 2 moves
the buffer base and ED 0 from home does not.

## 2026-11-01 run: the `ldap` client connector

Plan: the owner asked, in the project thread, for an LDAP client
connector on the `ldap3` crate, usable from dlua and from the command
line, with drt started with the connection settings; a later thread
ties it to JWT and topics. One step on claude/project-thread-qy9ase,
branched from PR #67 (claude/project-thread-hthum3) because the bind
password uses its `drt_config::Secret` and `src/prompt.rs`; the pull
request targets that branch and says so. No subagents.

Done:
- `connectors/ldap`: `ldap/bind`, `ldap/groups`, `ldap/search`. The
  scope holds url, base_dn, bind_dn, bind_password (a Secret, resolved
  once per process in `drt::ldap::connector`), starttls, extra_roots,
  user_filter, group_filter, group_base, timeout_ms.
- `drt ldap search|bind|groups` from `connectors.ldap` in `--config`,
  dispatched before `assemble` so a prompt is asked once.
- Tests: 4 unit, 12 against a fake directory (ldap3_proto), 5 against a
  real slapd (plaintext, StartTLS, ldaps, with a CA minted per test),
  4 through the binary. CI installs slapd and sets
  DRT_TEST_REQUIRE_LDAP=1, and checks `--features connector-ldap`.
- By hand, against a local slapd: `drt run` with a hidden-prompt
  service password under a pty, `drt ldap bind bob` asking for both
  passwords, and the no-terminal refusal under setsid.

Learned: `rmpv::ext::from_value` reads a unit enum variant only as an
index, so `{"prompt": "hidden"}` in a connector scope does not parse
through it; the ldap scope goes through serde_json instead. `host.ldap.*`
helpers do not exist: the per-family tables live in diluvium's host
library, so programs use `host.call` and `host.try`.

Next: the owner reviews; #67 merges first. The ruleset's required
checks do not yet name `features (connector-ldap)`.
## 2026-11-01 run: topics on the match server

Plan: the owner asked (project thread "Add topics to drt", working
autonomously while away) for topics: a namespace and access boundary
grouping kv and p2p names on the signalling server, with per-topic
tokens holding park, call, read and write, a members-only listing, and
today's bare `/v1/<name>` URLs kept working. One step on
claude/project-thread-2btm6m, ending in a pull request for review. The
kv store is a sibling thread; the seam agreed with it: kv lives at
`/v1/<topic>/kv/…`, its `kv_access(m, topic, op)` hook becomes
`topic_access` once both land.

Done:
- `drt p2p --match --topics <file>` (and `p2p.topics`): the file is
  checked in `matchmaker.rs` and handed to `stdlib/p2p_match.dlua` as
  JSON text. The program routes `/v1/<a>/<b>/…` three ways: bare name,
  topic listing (`GET /v1/<topic>/names`, read), name inside a topic.
  `may()` is the one place a token is judged, so a JWT verifier can
  replace it later.
- Pairing inside a topic: the told side calls with its own token, which
  must hold call; a bare name may not pair into a topic.
- Callers: `drt p2p` and the browser library read
  `drt://…/v1/<topic>/<name>`; the library escaped `lab/host` as one
  segment when following a pair entry, fixed and caught by a new leg of
  `pairing.mjs`.
- Verified: fmt; clippy, workspace, all features; `cargo test
  --workspace --all-features`; examples gate (31 ok, 2 skipped for
  network/privilege); `script/browser-access-client.sh --drt` with the
  new topic leg. Config corpus snapshots re-recorded: two lines,
  `topics: None`.

Next: once kv merges, wire `kv_access` to `topic_access`. Tokens are
plain strings in the file; `Secret` values (PR #67) could carry them.
## 2026-11-01 run: a kv store on the signalling server, and `drt kv`

Plan: the owner asked, in the project thread, for a kv store served by
`drt p2p --match`, with topics and LDAP as sibling threads. One step on
claude/project-thread-0zh3i9: the store in `stdlib:p2p-match` on the
`sql` connector (opt-in `--kv <dir>`), a `kv` connector over rest's
HTTP client, a `drt kv` verb, tests through the real server, an example,
docs and the changelog; then a pull request, not merged. The seam with
the topics thread (claude/project-thread-2btm6m): kv lives at
`/v1/<topic>/kv/...`, rows keyed by (topic, key), one `kv_route` call
from `route()`, and `kv_access(m, topic, op)` as the one admission point,
which topics fills in. No tokens here.

Done:
- `p2p_match.dlua`: kv tables, revision and the last 1024 changes kept in
  SQLite in one transaction per change; get/put/delete/list, If-Match and
  If-None-Match, wall-clock TTL with a sweep, watch as a stream and as a
  held poll, both resumable.
- `connectors/kv` on `drt_connector_rest::request` (new, public);
  `drt kv` on `p2p::http` (new `request_limited`).
- `tests/kv.rs` 14 passed, `connectors/kv` 4 passed, `examples/32-kv`
  ok; fmt and clippy with all features clean.

Learned: a browser's fetch folds `a/../b` before sending, so `.` and `..`
key segments are refused. Held polls and streams share the listener's 64
connections with parked peers.

Next: per-topic tokens (topics thread); `host.kv.*` sugar and `kv/*` in
the diluvium binary are diluvium's to add.
