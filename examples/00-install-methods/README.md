# 00-install-methods

Getting a drt binary, and proving it is the one you think. A drt binary is
one self-contained file with no runtime to install first (the Linux asset is
statically linked), so getting it is a download. The part worth teaching is
the step after: asking the binary what it is, instead of believing its
filename.

If you already have `drt` on your PATH, run the block below once and move on
to `01`.

## Run it

```sh
cd examples/00-install-methods
drt buildinfo
```

## What you should see

```
version: 0.4.0
profile: full
dv_abi: 1
dv_abi_expected: 1
connectors: time,fs,crypto,sql,ssh,rest,listen
verbs: buildinfo,netcheck,ps,relay,repl,run,start,stun,tunnel
```

That is this release; a different build answers with its own numbers. Six
lines come back, and each answers something you would otherwise guess:

- `version` — which release this is. A renamed file cannot lie here.
- `profile` — `full` or `slim`. Slim is the size build and carries fewer
  connectors, so a config that wires `ssh` fails on it at startup rather
  than at first call.
- `dv_abi` / `dv_abi_expected` — the diluvium ABI this binary speaks, and
  the one it was built against. When those two differ that is the thing to
  fix first, before reading any other error.
- `connectors` — what *can* be wired, not what is running. A config turns
  them on one at a time; a run with no config at all gets `time` and
  nothing else, which is what `01` shows. A connector missing from this
  line cannot be wired at any grant, whatever the config says.
- `verbs` — the subcommands this build has.

`drt buildinfo --json` is the same content on one line, which is the form a
package manager or a deploy script reads:

```
{"version":"0.4.0","profile":"full","dv_abi":1,"dv_abi_expected":1,"connectors":["time","fs","crypto","sql","ssh","rest","listen"],"verbs":["buildinfo","netcheck","ps","relay","repl","run","start","stun","tunnel"]}
```

Everything below this line is a recipe, not part of the run. It needs the
network, so the example gate does not execute it.

## 1. GitHub Releases — the path that works today

```sh
# Linux x86_64. The other published assets are drt_darwin_arm64 and
# drt_darwin_x86_64. The size profile is the same name with drt_slim_ in
# place of drt_, so: drt_slim_linux_static_x86_64.
BASE=https://github.com/Aloecraft-org/diluvium-drt/releases/latest/download
curl -fLO $BASE/drt_linux_static_x86_64
curl -fLO $BASE/SHA256SUMS.txt
sha256sum --ignore-missing -c SHA256SUMS.txt     # shasum -a 256 -c on macOS
chmod +x drt_linux_static_x86_64
mv drt_linux_static_x86_64 ~/.local/bin/drt      # anywhere on PATH
drt buildinfo
```

The checksum proves the bytes arrived as published. It says nothing about
what is inside them, which is why the recipe ends at `buildinfo` rather than
at `--version`: the sums file answers "did this download correctly" and
`buildinfo` answers "is this the build I need".

`install.sh` ships as a release asset beside the binaries and does the same
download, the same check, and a note about PATH:

```sh
curl -fsSL $BASE/install.sh | sh
```

It reads `DRT_SLIM=1` for the size profile, `DRT_VERSION=vX.Y.Z` to pin a
release, and `DRT_PREFIX=` to choose the install directory.

Releases are listed at
<https://github.com/Aloecraft-org/diluvium-drt/releases>.

## 2. One thing that does not exist yet

dollup is the intended front door for fetching and verifying Aloecraft
binaries, and it is not released yet. Nothing on this page uses it or waits
for it; the recipes here are the whole story today and will keep working.

## 3. Installing with no network at all

`DRT_MIRROR` overrides where `install.sh` looks for binaries, and it takes a
`file://` URL. That is the air-gapped install, with no special code path:

```sh
DRT_MIRROR=file:///mnt/xfer/drt DRT_VERSION=v0.4.0 sh install.sh
```

The directory needs to hold `<version>/drt_<os>_<arch>` — here
`v0.4.0/drt_linux_static_x86_64` — and `SHA256SUMS.txt` alongside it if you
want the check to run. Carry `install.sh` across on the same medium. Nothing
in this reaches the network.

A missing sums file warns and continues rather than refusing, so a mirror
you assembled by hand still installs. A sums *mismatch* always refuses.

## 4. The mirror URL

`https://diluvium.aloecraft.org/release/drt/install.sh` is the intended
front door, and `install.sh` itself prefers that mirror when resolving
binaries. It does not carry the `drt` namespace yet: as of this release that
URL 404s, as do `/release/drt/latest/…` and `/release/drt/releases.json`.
Use the GitHub Releases URLs above until it lands.

The script already handles this. It tries the mirror first, falls back to
GitHub Releases when the mirror does not answer, and prints which source it
used — so the same one-liner keeps working on the day the mirror starts
carrying drt, with nothing for you to change.
