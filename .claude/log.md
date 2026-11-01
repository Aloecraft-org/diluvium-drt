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

## 2026-11-01 run: TOTP and EdDSA in the crypto connector

Plan: the owner asked, through the project coordinator, for TOTP and
asymmetric JWTs in `crypto`, feeding a session framework (LDAP bind, a
second factor, a session, forward-auth, JWTs) and a JWKS served by a
program. One step on claude/project-thread-qq22id: three TOTP verbs
(generate, verify with a window and a replay step, enroll), EdDSA chosen
per key (a derived label or a configured PKCS#8 key in the secret-value
shape of PR #67), `public_key` as a JWK, then RFC vectors, interop with
independent libraries, an example, docs, changelog, a pull request.

Done:
- `crypto/totp`, `crypto/totp_verify`, `crypto/totp_enroll`; seeds are a
  label's third subkey (`KDF_LABEL_TOTP`) or a `crypto.secrets` entry,
  which never enrols.
- `derive {alg = "EdDSA"}` (`KDF_LABEL_EDDSA`), `crypto.signing_keys`,
  EdDSA in `jwt_sign`/`jwt_verify` with the algorithm taken from the key,
  `crypto/public_key`.
- Tests: RFC 4226/6238/8037 vectors; totp-rs and jsonwebtoken as
  dev-dependencies; alg-confusion forgeries. By hand, PyJWT and pyotp
  against a real `drt run`. Example 32, corpus entry, GUARANTEES and the
  changelog.

Learned: a bare `*` in a derive scope names only the label `*`, by
design (`drt_caps::implies` refuses a zero-length prefix so a grant
cannot widen); grant labels by name or by a prefix like `issuer:*`.

Next: the plain Diluvium host (`dhost_crypto.c`) answers none of these
calls; TOTP there is cheap (dhash.h has HMAC-SHA1), EdDSA is not.
