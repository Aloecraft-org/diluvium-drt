# 00-install-methods

A drt binary is one self-contained file with no runtime to install beside it
(the Linux asset is statically linked; the darwin ones are not), so getting
it is a download. The step after is the one worth learning: asking the
binary what it is, instead of believing its filename.

## Run it

```
cd examples/00-install-methods
drt buildinfo
```

## What you should see

```
version: 0.4.0
profile: full
dv_abi: 1
dv_abi_expected: 1
diluvium: f137b308c4dce917b24c71ab41add61606945e58
connectors: time,fs,crypto,sql,ssh,rest,ssmtp,exec,listen
verbs: buildinfo,netcheck,ps,relay,repl,run,start,stun,tunnel
```

Another build answers with its own numbers. `drt buildinfo --json` is the
same content on one line, which is the form a deploy script reads.

## What it teaches

- **`profile` decides what you can wire.** Slim is the size build and
  carries fewer connectors, so a config that wires `ssh` fails on it at
  startup rather than at first call.
- **`connectors` is what *can* be wired, not what is running.** A config
  turns them on one at a time, and a connector missing from this line cannot
  be wired at any grant, whatever the config says.
- **`diluvium`** — the git revision of the language core inside. A
  revision, not a version: the core exposes no version string, and what has
  actually mattered between the two — whether a named defect is present —
  is a revision fact. This is what a package's `requires.diluvium` is
  checked against.
- **`dv_abi` against `dv_abi_expected`** — the diluvium ABI this binary
  speaks, and the one it was built against. When those two differ, fix that
  before reading any other error.

## Getting a binary

This part needs the network, so it is not part of the run above.

```
# example: omits the other published assets — drt_darwin_arm64,
# drt_darwin_x86_64, and drt_slim_* for the size profile.
BASE=https://github.com/Aloecraft-org/diluvium-drt/releases/latest/download
curl -fLO $BASE/drt_linux_static_x86_64
curl -fLO $BASE/SHA256SUMS.txt
sha256sum --ignore-missing -c SHA256SUMS.txt  # shasum -a 256 -c on macOS
chmod +x drt_linux_static_x86_64
# example: omits making ~/.local/bin and checking it is on your PATH.
mv drt_linux_static_x86_64 ~/.local/bin/drt
drt buildinfo
```

The checksum proves the bytes arrived as published and says nothing about
what is inside them, which is why the recipe ends at `buildinfo` rather than
at `--version`.

`install.sh` ships beside the binaries, under that same `$BASE`, and does
the same download and the same check. Point `DRT_MIRROR` at a `file://`
directory laid out like a release — `v0.4.0/` holding the asset and its
`SHA256SUMS.txt` — and it installs with no network at all.

```
# knobs: DRT_VERSION pins a release, DRT_PREFIX picks the directory,
# DRT_SLIM=1 takes the size profile.
curl -fsSL $BASE/install.sh | sh
DRT_MIRROR=file:///mnt/xfer/drt DRT_VERSION=v0.4.0 sh install.sh
```
