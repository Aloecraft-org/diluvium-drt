# The ported capability suite

discofetch's `capability_testing/` slices, ported to run against DRT. Each
upstream slice is a container that drives the **real shipped runtime** and
proves exactly one thing; each port here is an integration test driving the
real `drt` binary and proving the same thing.

| upstream | here | status |
|---|---|---|
| `cap1_environment` | [`cap1_environment.rs`](cap1_environment.rs) | ported |
| `cap2_sqlite_json` | partly, in [`connectors/sql`](../../../connectors/sql/tests/scope.rs) | the workload round-trips; the slice's own shape lands with `drt start` |
| `cap3_crypto_jwt` | — | blocked: no `crypto` connector yet |
| `cap4_swarm` | — | the swarm and `sql` are both in; the slice's supervisor shape lands with `drt start` |
| `cap5_ports_daemon` | — | blocked: no `listen` connector / `drt start` yet |
| `cap6_fs` | partly, in [`connectors/fs`](../../../connectors/fs/tests/jail.rs) | the verb surface round-trips; the slice's own shape lands with `drt start` |
| `cap7_plugins` | — | out of scope until dynamic connector loading (SPEC.md §7, a seam) |

## Porting rule

Port the *claim*, not the transcript. Where DRT is deliberately different
from the runtime a slice was written against, the port asserts the property
the slice was reaching for and records why the literal check does not
apply — never quietly drops it, and never pretends the difference is not
there.

cap1 is the worked example: upstream it isolates the two-binary cliff
(`diluvium` from the installer, `diluvium-host` built separately and shipped
by nobody), because which binary you have is the first thing to get wrong.
SPEC.md §1 designs that cliff away — DRT embeds the language, so there is
one binary and installing it gets you everything. The port asserts *that*,
and pins it so it cannot quietly stop being true.

Differences found while porting, recorded rather than smoothed over:

- The embedded engine answers `_VERSION` as `diluvium (lua) 5.5`, where the
  plain CLI upstream answers `Lua 5.5`. The embedded one names itself, which
  is the more precise answer.
