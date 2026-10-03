# DRT

Orientation for an agent working in this repository. The owner is
Michael (`@aloecraft`). The operating protocol is
`.claude/rules/operating.md`; it and every other file in
`.claude/rules/` apply to every run, and what the owner and operator
have agreed lives in `doc/lockstep/`. Read `doc/Handoff.md` next: it
says where the work stands, which branch holds it, and what is next.
This file says how the repository works.

**This repository is public.** So are `.claude/log.md`, everything
else under `.claude/`, and `doc/lockstep/` (goal, queue, ledger). The
owner accepts that; write them knowing anyone can read them, and keep
anything private to the DiRT launcher or other private repos out.

## What this is

DRT is a portable runtime for Diluvium programs (sandboxed Lua plus
additions) under a capability model. One `drt` binary runs programs,
serves deployments, and carries their traffic: a relay, STUN, WireGuard,
SSH, and WebRTC peer-to-peer with a reference signalling server. The
same runtime builds for `wasm32-wasip2` and for the browser (`drt-web`).
The Diluvium core is a pinned C library (`diluvium-sys`, see the `tag`
in `Cargo.toml`); DRT is the host around it.

`SPEC.md` is the founding spec. `GUARANTEES.md` is normative and CI
checks it exists. `README.md` and `examples/` are the user-facing
surfaces.

## Layout

- `crates/drt` the binary: CLI (`src/cli.rs`), p2p (`src/p2p/`), the
  control endpoint (`src/control.rs`), the REPL, the stdlib programs
  (`src/stdlib/*.dlua`), and the integration tests (`tests/`).
- `crates/drt-swarm` the engine around the C core. `drt-caps`
  capabilities. `drt-config` the config types and resolution; its JSON
  schema is generated to `doc/drt-config.schema.json`.
- `crates/drt-rtc` WebRTC and the Wisp stream protocol (`wisp.rs`),
  host and caller. `drt-sshd` the SSH server shared by native and page.
  `drt-ssh-web` the SSH client for a page.
- `crates/drt-web` the browser build, with its Chromium suite in
  `browser-test/`. `crates/drt-plugin` out-of-process connector plugins,
  wired behind the non-default `plugins` feature.
- `connectors/*` one crate per hostcall family (fs, sql, ssh, rest, ...).
- `examples/` numbered walkthroughs; each has a `meta.json` and an
  `expected.txt` that `examples/run-all.sh` diffs against.
- `tests/determinism` a corpus the same gate runs on every target.
- `script/` build, gate and release helpers. `bench/` the fidelity
  benchmark CI compares against a C baseline.

## Build and test

The `drt` crate's default feature is `slim`. Most work wants `full`.

```sh
cargo build -p drt --features full
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace                     # default features
cargo test --workspace --all-features      # what covers the code
script/config-schema.sh --check            # after touching drt-config
cd examples && DRT=../target/debug/drt ./run-all.sh
```

Build the examples binary with `--features full`, not
`--all-features`: `plugins` makes the profile `custom`, and the gate
skips every example that needs `full`.

Heavier gates, each one a CI job (see `.github/workflows/ci.yml`):

```sh
WASI_SDK_PATH=<wasi-sdk 27> script/drt-web.sh   # rebuild the page's wasm
script/drt-ssh-page.sh                          # the page's SSH client
script/browser-access-client.sh --drt           # Chromium, ~44 checks
script/drt-ssh-page-gate.sh
```

Tool pins: wasi-sdk 27, `wasm-bindgen-cli` equal to the `=` pin in
`crates/drt-web/Cargo.toml`, Node with Playwright's Chromium.

The SSH tests start a real `sshd` as root through `sudo -n`. They need
`openssh-server` and `/run/sshd`. Without those they print `skipped` and
pass, unless `DRT_TEST_REQUIRE_SSH=1` is set, which CI sets.

## Conventions

- **Commits.** Subject `area: what changed, as a sentence`. The body
  says why, in prose, and names what was verified. Small, validated
  pushes. Never push a red tree to see what CI says.
- **Changelog.** Edit `CHANGELOG.yaml` only, then
  `python3 script/changelog.py generate`; `CHANGELOG.md` and
  `changelog.json` are generated and `check` fails if they drift. The
  newest entry is `status: unreleased` until a release is cut.
- **Docs say what is built.** A doc that promises an unbuilt thing says
  "not built" beside it. When code changes a fact, fix every doc that
  states the fact in the same commit. The release QA on 2026-10-02 found
  a dozen docs describing things that had changed under them.
- **Human surfaces.** `.claude/rules/human-surfaces.md` (shared,
  placed by technoproj) governs them. The declared surfaces are
  `README.md` and `examples/`, set in `.technoproj`; every prose file
  outside `.claude/` counts as documentation under that rule.
- **Pull requests.** Changes reach `main` through pull requests, never
  a direct push outside the self-merge lane in
  `doc/lockstep/authority.yaml`.
- **Release machinery.** `doc/Release.md` is the procedure. Dev builds
  are `v<version>-dev.<n>`, allocated by `script/dev-tag.sh`, never
  reused. `release.yml` takes `full` to build every target for a dev
  build; a release always builds every target.

## Things that cost time before

- A test that needs a port nothing listens on must not bind `:0` and
  drop it: parallel tests take the freed port. Use a refusing low port.
- After changing anything the page prints, rebuild the wasm before
  trusting a local browser gate. A stale `pkg/` passes checks that CI
  then fails.
- The three workspace feature sets each produce a full target tree.
  Running them back to back in one target directory fills a cloud
  session's disk. Build one set at a time, or clean between them.
- A `workflow_dispatch` runs the workflow file of the ref it is given.
  `dev-build.yml` dispatches `release.yml` on the default branch, so a
  release-workflow change only reaches dev builds after it is on main.
