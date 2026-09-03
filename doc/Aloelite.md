# Aloelite and DRT: how they should meet

**Status:** assessment, written 2026-09-03 against DRT v0.4.2 and
aloelite's Rust port on `claude/aloelite-rust-port-assessment-732bo0`
(workspace `0.4.0-rc.1`, six crates), before that port has had its
evaluation. Claims about aloelite name the file in its tree; claims
about DRT name the file here or on the wasm branch. Nothing is built.

**The ask.** Aloelite is a filesystem inside one SQLite file: nodes,
edges, volumes and mounts, content-addressed chunks with deduplication,
at-rest encryption per volume, and a Mount API of about fifty operations
(`aloelite/config/mount-api.yaml`). Today it is already useful to DRT by
mounting a volume over FUSE and pointing the `fs` scope at the mount.
Which of the ways to integrate is right: build it in, a plugin on
aloelite's operations, some other plugin, S3, WebDAV, FUSE?

---

## 1. The verdict

**Build it in, as a backend of the `fs` connector, and later as a
backend of the snapshot store.** Not as a new hostcall family first, not
as a plugin, and not through S3 or WebDAV.

The reason is what the Rust port is. `aloelite-core` compiles to native,
`wasm32-wasip2` and `wasm32-unknown-unknown` with zero `cfg`, performs no
I/O of its own, and takes a `rusqlite::Connection` someone else opened
plus a clock and an entropy source (`rust/aloelite-core/src/lib.rs`,
`rust/README.md`). That is exactly the shape of a leaf DRT can sit on
every target. The wasm branch's `drt-platform` already splits the `fs`
connector into a jail written once and a `Backend` with six operations
(`crates/drt-platform/src/fs.rs`: `canonicalize`, `metadata`, `read`,
`write`, `read_dir`, `remove_file`), with `StdFs` for a disk and `MemFs`
for a page. An aloelite volume is a third backend of that trait, and
then a program reading `fs/read` cannot tell a directory from a volume,
which is DRT's doctrine applied to storage.

FUSE, WebDAV and S3 are three ways of making a volume look like a place
DRT can already reach. A backend makes DRT reach the volume itself, on
the two targets where the other three cannot follow.

## 2. The options, against the tree

| route | what DRT does | targets | cost | verdict |
|---|---|---|---|---|
| **FUSE mount, `fs` scope on it** (today) | nothing | Linux only; needs `/dev/fuse`, a daemon to supervise | zero | keep; it is the deployment shape until the backend exists, and stays right where an operator wants the volume visible to other processes |
| **`fs` backend over `aloelite-core`** | a third `Backend` behind the jail; the scope names a volume file, a volume, a mount point and a PIN source | native, wasip2, browser | ~2-3 days after the wasm branch's M2 merges; dependency alignment (§4) | **first** |
| **snapshot store over a volume** | a second `SnapshotStore` (`crates/drt-swarm/src/snapshot.rs`: put/get/remove/list) | the same three | ~1 day | **second** |
| **an `aloe/*` hostcall family** | the operations `fs/*` cannot say: metadata, versions, pack and unpack, locks, verify | the same three | ~2-3 days for a handful of verbs; the whole API could be generated from `mount-api.yaml` | later, by demand |
| **a plugin on aloelite's operations** | the Mount API behind the plugin channel | native, then wasip2 over `tcp` | the channel first (`doc/Plugins.md`), then a process for what is a library | no: a plugin is for what cannot be linked, and this can |
| **WebDAV** | a new `dav` connector: PROPFIND, PUT, MKCOL, MOVE, LOCK; `rest` speaks GET and POST only | native and, later, wasip2 | moderate | only for a *remote* volume behind the manager; not the local story |
| **S3** | a new `s3` connector with SigV4 and multipart; aloelite's endpoint is litestream's eight calls (`doc/S3.md` there) | native and, later, wasip2 | a general S3 connector, valuable on its own, a detour here | not for this |

## 3. The shape of the backend

The scope is the place, as every DRT scope is:

```json
"fs": {
  "scope": {
    "volume_file": "notebook.fs",
    "volume": "docs",
    "mount_point": "/",
    "pin_env": "NOTEBOOK_PIN",
    "access": "readwrite",
    "max_bytes": 1048576
  }
}
```

- **`volume_file`, `volume`, `mount_point`** are the directory, one level
  richer. The connector opens the file, mounts the volume at the mount
  point, and the jail resolves every program path inside that mount, as
  it resolves inside a directory today. The mount is an ACC-1a mount row
  (`doc/REQUIREMENTS.md` there): access is brokered, never ambient, and
  the mount has a TTL the connector renews on use.
- **`pin_env` or `pin_file`**, never `pin`, the way `crypto` names its
  key: the PIN is the deployment's and reaches no guest. A wrong PIN is
  refused at startup, by name, because aloelite refuses it at mount and
  not at first read.
- **`access` and `max_bytes`** keep their meaning. A volume is a place
  that happens to be encrypted and deduplicated; the program's contract
  does not change.
- **Unreachable fails at startup.** A file that is not there, a volume
  the file does not hold, a PIN that does not open it, an era the Rust
  engine refuses (`doc/RUST_PORT.md` there records that it may refuse
  era-1 files): all named before the first step.

The six backend operations map without invention: `canonicalize` is
lexical over a tree with no symlink resolution to do (a symlink node is
era 2's, and the backend either follows it inside the mount or refuses
it; decide, and the jail's second check still applies), `metadata` is
`stat`, `read` is `read_all`, `write` is `write_all` or `append`,
`read_dir` is `list`, `remove_file` is `remove`. Every one is atomic in
aloelite's contract (TX-1), which is stronger than what the disk gives.

**What it buys beyond parity.**

- **A deployment can be one file.** On the wasm branch the program and
  config loader read through the same backend as the connector. With a
  volume behind it, config, program, workspace and, with the snapshot
  store, the hibernated agents live in one encrypted, deduplicating,
  portable file. SPEC.md §10's "a snapshot restored in another
  process or machine next week" becomes copying that file.
- **The browser tier gets persistent files.** `doc/Wasm.md` §7 leaves
  "persistent files in the page" open with OPFS as the natural second
  backend. `aloelite-store` already opens a volume over OPFS in a
  Dedicated Worker, synchronously, which is the only shape a
  synchronous `Backend` can use, and DRT's module already runs in that
  worker.
- **wasip2 gets a filesystem that is not a preopen.** A volume file on
  the one `--dir` wasmtime grants, and everything inside it reaches the
  program through the jail.
- **Snapshots deduplicate.** Agents parked in similar states share
  chunks; a volume of a thousand hibernated instances costs what their
  differences cost.

## 4. Costs and hazards, stated

- **SQLite is linked once or not at all.** DRT's `sql` connector pins
  `rusqlite 0.37`; aloelite-core pins `0.40`. `libsqlite3-sys` carries a
  `links` key, so two versions in one build do not compile. The `sql`
  connector moves to 0.40, which is half a day and should happen first.
  After that the bundled SQLite `full` already carries serves both, and
  aloelite costs `full` only its own code and the RustCrypto ladder.
- **`slim` stays without it.** No SQLite in `slim` is a decision
  (`connectors/sql/Cargo.toml`), so the backend is a feature the `full`
  and `wasi` profiles carry and `web` decides on separately, where it
  costs SQLite compiled to wasm.
- **`ego_platform` comes along.** aloelite-core takes its clock from
  `ego_platform`, which brings tokio natively and pins `wasm-bindgen`
  at `=0.2.114` while the wasm branch moved DRT to `0.2.127`
  (`doc/Wasm.md` §7). The wasm branch already records the pin ask
  upstream; the cleaner fix is an aloelite-side one: a clock trait of
  its own, so a consumer supplies the clock and `ego_platform` is not
  in the engine's dependency graph. Small, and worth asking for before
  the port is evaluated.
- **Versions travel with the bytes.** aloelite has a schema era and a
  Mount API version; DRT's rule is that the compatibility fact rides in
  BUILDINFO. A build carrying the backend reports the era it opens, the
  way it reports `dv_abi`.
- **Two writers, one file.** A FUSE daemon or the manager may hold the
  same volume open while DRT does. SQLite in WAL handles the
  processes; aloelite's mount admission policy (`doc/DECISIONS.md`
  D-4 there: one read-write mount per subtree unless overlap is opted
  in) handles the mounts, and the connector's mount is subject to it
  like any other. A refused mount is a startup refusal by name.
- **Keep volumes out of `sql` scopes.** A volume is a SQLite file; a
  program holding `host:sql/*` on the directory that holds it could open
  the volume as a database and read node metadata, which aloelite stores
  in plaintext (its README's security notes say so). Scope hygiene, and
  the startup validation could refuse the overlap.
- **The port is untested here.** The branch says the conformance suite
  passes on native and in a browser; that is the port's claim, not DRT's
  measurement, and nothing above should land before the evaluation.

## 5. What is not this document's

The `aloe/*` family's exact verbs, which want the demand first. The
manager's HTTP API as a remote target, which is a `dav` or `s3`
connector question and a general one. And the pack format, which is
aloelite's cross-implementation wire format and none of DRT's business.
