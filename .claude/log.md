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

## 2026-11-01 run: a JWKS from a topic's kv, `stdlib:jwks`

Plan: the owner asked (project thread "Publish JWKS from topic kv", while
away) for JWT verification keys published through a JWKS endpoint built
on topics and kv, with rotation and kid selection, private keys never
leaving the host, and tokens verified by a standard library. One step on
claude/project-thread-0s4wb2, stacked on three unmerged branches merged
in: topics (2btm6m, #68), kv (0zh3i9, #69) and crypto EdDSA +
`public_key` (qq22id, #71). Then a pull request, not merged.

Done:
- The topics/kv seam, filled here because both are on this branch:
  `kv_access` asks `topic_access` with the op as the role when
  `--topics` is given; kv's dispatch sits ahead of the topic split in
  `route()`; the docs that said kv takes no token say when it does. The
  crypto example renumbered 32 -> 33 (kv keeps 32). `Command::P2p` boxed:
  topics + kv flags tripped `clippy::large_enum_variant`.
- `stdlib/jwks.dlua`: publishes each `args.keys` name's JWK under
  `jwks/<kid>` (`~XX`-escaped) with `?ttl=`, refreshed every ttl/3, and
  serves the topic's set at `/.well-known/jwks.json`, filtered to public
  members, one key per kid; 503 when kv does not answer. The same set is
  kept as one kv value under `jwks` (`args.set_key`), the contract the
  "Tie LDAP groups to topic tokens" thread's match server reads.
- `tests/jwks.rs` (6, jsonwebtoken), `examples/34-jwks` (PyJWT's
  `PyJWKClient` over HTTP; CI's examples step installs PyJWT 2.15.0).

Learned: `drt start` with a listener exits 0 when its root instance
faults (start.rs, `alive == 0`), so a supervisor cannot tell a fault
from a chosen exit. Not changed here.

Next: `crypto/jwt_verify {token, jwks}` (a verifier that holds no master)
is on the "Tie LDAP groups to topic tokens" branch (z0edkg), not this one.
A doc of its own for the JWKS waits for the owner to ask for one.

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
  against a real `drt run`. Example 33 (32 is kv's), corpus entry, GUARANTEES and the
  changelog.

Learned: a bare `*` in a derive scope names only the label `*`, by
design (`drt_caps::implies` refuses a zero-length prefix so a grant
cannot widen); grant labels by name or by a prefix like `issuer:*`.

Next: the plain Diluvium host (`dhost_crypto.c`) answers none of these
calls; TOTP there is cheap (dhash.h has HMAC-SHA1), EdDSA is not.

## 2026-11-01 run: `stdlib:session`, the session framework

Plan: the owner asked, through the project coordinator, for an early
Authelia-like session framework: LDAP bind, a second factor (TOTP or a
generic webhook), a session with a TTL in a kv topic, a cookie, a
forward-auth `/verify`, and a JWT from the session; no password hashing,
no OIDC or WebAuthn. One step on claude/project-thread-0c13ig, stacked
on the unmerged #67 (via #70), #68, #69 and #71, merged in that order. A
dlua stdlib program, host pieces only if needed (none were). Tests
against a real slapd and kv server, and Chromium through Caddy. No
subagents.

Done:
- The stack: kv's `kv_access` asks `topic_access` when `--topics` is on
  (the seam both PRs named); the topics test reads `/v1/<t>/kv/...` as
  kv; `Command::P2p` boxed for clippy; the feature-leaves count; docs
  that said kv takes no token.
- `crates/drt/src/stdlib/session.dlua`, `stdlib:session`, gated on
  ldap+kv+crypto+time+listen.
- `crates/drt/tests/session.rs` (8 tests) and `tests/kv.rs` (topic
  tokens on kv); `script/session-browser.sh` (13 checks) and its CI job.
- `doc/Session.md`, README, changelog.

Learned: the args message carries the `from` envelope, so a program
checking for unknown arguments must skip it. `drt run` does not deliver
`args` to a `program.path` config. A Caddy site address with a host only
matches that Host header, and reverse_proxy keeps the original Host. The
login pages' CSP (`default-src 'none'`) also blocks a test's injected
fetch. `drt start` exits 0 when the root program faults.

Next: JWKS (the public key is `crypto/public_key` on `jwt_key`), the
JWT-to-topic-token tie-in, and serving more than one request at a time.
## 2026-11-01 run: LDAP groups as topic roles, through JWTs

Plan: the owner asked (via the project coordinator, while away) to tie
JWT to LDAP and LDAP to topics for kv and p2p. One step on
claude/project-thread-z0edkg, stacked on every unmerged branch it needs:
merge topics (#68), kv (#69), config and ldap (#67, #70) and crypto
(#71); give crypto a public-key-only verify; let the match server's one
token judge (`may`) admit a JWT whose groups the topics file maps to
roles; test end to end against a real slapd; open a pull request.

Done:
- Merges, with the two seams the sibling PRs named: `kv_access` asks
  `topic_access`, and `route()` sends kv first. Two merge faults found
  by tests and fixed: a lost `end` in `topic_names`, and `Command::P2p`
  tipping clippy's large_enum_variant (boxed).
- `crypto/jwt_verify {token, jwks}` and the `{"verify_only": true}`
  scope; 8 tests in `connectors/crypto/tests/jwks.rs`.
- `principal(topic, token)` in `p2p_match.dlua`: a listed token, else a
  JWT checked against the JWKS (inline, or a kv key re-read per kv
  revision), `iss`, `aud`, `sub`; roles are the union over its groups.
  Names are held by principal (`jwt:<sub>`), so a renewed token keeps
  the name.
- `tests/topics_jwt.rs`: slapd, a dlua issuer under `drt run`, real
  park/call and kv by group, rotation by kv write, forged and tampered
  tokens, issuer/audience/expiry. A mutation granting every JWT every
  role fails it.

Learned: naming any grant for the root makes its grants explicit, so
the match server must name `host:time` beside `host:crypto/jwt_verify`
(kv already did). The kv merge also turned `/v1/<topic>/kv/...` into
kv's path even without `--kv`, so the topics test now expects 404 there.

Next: the session framework issues these tokens with claims `iss`,
`aud`, `sub` (the LDAP username) and `groups` (`ldap/bind`'s answer),
and publishes `crypto/public_key` as `{"keys": [...]}` at a kv key.
Renewal on a parked side is not built.

## 2026-11-01 run: the auth stack integrated, #67 to #74 together

Plan: merge the eight auth PRs onto one branch from main in dependency
order (#67 config, #70 ldap, #68 topics, #69 kv, #71 crypto, then #72
jwks, #73 session, #74 topics-jwt), resolve where their separate merges
of each other disagree, run every gate with all features and with
default features, and write one end-to-end test that does what a person
would. Fixes that belong to one PR go to that PR's thread; the
integration PR stays unmerged for the owner.

Done:
- Branch claude/project-thread-kj0elp. Each merge is its own commit.
  Seams: kv_access asks topic_access the same way in #72, #73 and #74
  (only comments and one log line differ; #73's kept, #72's test
  matched to it); #74's principal() kept for names and kv; the crypto
  example is 33, after kv's 32, as #72 named it; one base64
  dev-dependency where #72 and #74 each added one; the slapd AppArmor
  fix (#70 d60fa9c, #73 1165590) is the same copy-the-binary fix in two
  test files and both are kept, since each file starts its own slapd.
- CHANGELOG.yaml rebuilt from each PR's items: the text merges had
  glued items together and duplicated kv's. #74's branch has the TOTP
  item glued onto the --forward one by itself; sent to its thread.
- crates/drt/tests/auth_stack.rs: slapd, one match server with kv and
  topics, stdlib:session and stdlib:jwks on one master, a browser
  signing in with TOTP computed in the test, the session JWT parking,
  calling and using kv in `home` by group, sessions in `auth` out of a
  user's reach, PyJWT verifying through the JWKS URL. Mutations (staff
  without call; wrong groups expected from PyJWT) fail it.
- CI installs PyJWT in the test job and sets DRT_TEST_REQUIRE_PYJWT.
- doc/Session.md and doc/P2P.md no longer say the JWKS endpoint or the
  session framework is unbuilt.
- Gates: fmt, clippy all features, config schema, changelog
  check/consistency, cargo test all features and default features
  (with DRT_TEST_REQUIRE_SSH/LDAP), the examples gate on a full build.

Found, not fixed here: crypto's signing_keys secret parser duplicates
drt_config::Secret and refuses prompt, where ldap's bind_password takes
it (sent to the crypto thread). A session JWT outlives a logout until
its own exp; the test states that, and jwt_ttl is the knob.

## 2026-11-01 run: JsConnector and net (#80), sql.js in a page (#81)

Plan: two pull requests. PR 1 (claude/project-thread-wo5zam): a `net`
connector in `connectors/net` over a host-supplied `Transport`, with the
socket connector's handle rules (owner, release, transfer/claim, vital)
and two inbound shapes (push to a named queue, or buffer for recv and
accept); a `Supplied` seam in `cli::wire_connectors_with` and
`assemble_with` so a host can hand in a connector for a family the build
does not carry; `drt-web/src/js.rs` turning a page's `{family: object}`
into connectors (`net` a transport, anything else `{call}`), per kernel
through `DrtTerm(sink, backings)` and `DrtSwarm(..., backings)`; drt.js
`kernel()` and `bus()`. PR 2 (claude/project-thread-wo5zam-sqljs, on PR
1): `sql` in a page over sql.js with the native contract, databases kept
as files in the page's filesystem so examples/07-sql runs unchanged in
the browser. #81 route 2 (bundled SQLite for wasm32-unknown-unknown)
stays parked. #82 is PR #83's; backings are per kernel so they fit it.
