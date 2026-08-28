# Releasing DRT

The strategy in one sentence: **DRT versions independently of diluvium,
records the coupling instead of encoding it, and lands on the same release
mirror as a sibling namespace** — so Lab, the deploy scripts and the
install one-liner all keep reading one host with one layout.

## Versioning: independent, with the coupling recorded

DRT tags its own `vX.Y.Z`. A DRT release does not name a diluvium release
— it *embeds* a diluvium revision (the git dependency in `Cargo.lock`) and
speaks one dv ABI version, and both facts are stamped into the release's
`BUILDINFO.txt`. "Which diluvium is inside" is something you read off the
artifact, never something you infer from a tag-naming convention. When a
DRT release happens to embed a published diluvium tag, the mirror's index
can say so; when it embeds a plain revision, that is equally fine and
equally recorded.

This is SPEC.md's version-first doctrine applied to distribution: the
compatibility fact travels with the bytes.

## The workflow

`.github/workflows/release.yml`, shaped like diluvium's on purpose:

- **Push a tag `v*`** → tests, builds every platform, publishes.
- **Actions → Release → Run workflow** → a full rehearsal by default:
  tests and all platforms, artifacts left on the run, nothing published
  until `publish=true`.

Artifacts use the mirror's flat naming, with `full` as the unprefixed
default — DRT installs as *the runtime*, and a runtime that cannot serve
a fetchpoint is the two-binary cliff again:

```
drt_linux_static_x86_64        # full: engine, connectors, listen, tunnel
drt_slim_linux_static_x86_64   # slim: the distribution-size profile
drt_darwin_arm64               drt_slim_darwin_arm64
drt_darwin_x86_64              drt_slim_darwin_x86_64
BUILDINFO.txt                  SHA256SUMS.txt
```

Linux aarch64 and Windows are next, not promised: `full` carries
`aws-lc-sys` through russh, and cross-compiling that is a thing to
rehearse (the workflow's dispatch mode exists for exactly this), not to
assume. The changelog-as-gate machinery diluvium's release carries is
worth adopting once DRT has releases worth gating; it is deliberately not
cargo-culted in on day one.

## The mirror (the server-side half)

One ask outside this repo: run the existing mirror generator a second
time, pointed at `Aloecraft-org/diluvium-drt`, into a sibling namespace —

```
https://diluvium.aloecraft.org/release/drt/<tag>/…
https://diluvium.aloecraft.org/release/drt/latest/…
https://diluvium.aloecraft.org/release/drt/releases.json
```

Same per-tag directories, same `latest/` stable path, same
`releases.json` shape (plus a `diluvium` field per release carrying the
embedded revision from `BUILDINFO.txt`). Lab and every deploy script then
learn one new path segment and nothing else.

## Building for wasm

`--no-default-features` carries the swarm and the capability layers without
the C core, and that is what a browser build starts from — there the
`Engine` bridges to a JS-hosted diluvium instance rather than linking one
(SPEC.md §4).

Building the `engine-diluvium` feature *for* a wasm target is the other
case, and it needs a wasi-sdk (>= 24) named by `WASI_SDK_PATH`, because the
C core must be compiled by a clang that can target wasm32. Without it
`diluvium-sys` refuses with an explanation. That refusal is deliberate and
worth knowing about: until recently the build *succeeded* and emitted a
host `.o`, so `cargo build --target wasm32-unknown-unknown` looked green and
only failed when something forced an actual link.

## Installing

`install.sh` at the repo root, served from the mirror:

```
curl -fsSL https://diluvium.aloecraft.org/release/drt/install.sh | sh
```

Mirror-first with a GitHub Releases fallback, SHA-256-verified when the
mirror's sums are reachable, `DRT_SLIM=1` / `DRT_VERSION=vX.Y.Z` /
`DRT_PREFIX=…` as the knobs.

**On DRT as the primary install candidate: yes, staged.** The argument
for: one binary that runs a script (`drt run`), serves a deployment
(`drt start`), reads the C host's own configs, and tunnels (`drt
tunnel`) covers everything a newcomer reaches for plus the runtime story,
while the `diluvium` interpreter remains the compiler/embedding artifact.
The staging: keep `/start` pointing at diluvium until discofetch runs on
DRT in production; flip it after — an install one-liner should hand out
the thing production has proven, and that proof is days away, not months.
