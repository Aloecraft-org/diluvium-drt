# Golden fixtures

Shared bytes, so drt and dollup assert against the same thing rather than
each writing its own expectation and diverging. `dollup audit` disagreeing
with `drt start` is the failure these exist to make impossible, and
`canonical.json` plus `canonical.sha256` is the pair that prevents the
"signature failures nobody can debug" class outright.

| file | what |
|---|---|
| `canonical.json` | the canonicalizer's input |
| `canonical.bytes` | the exact canonical bytes, no trailing newline |
| `canonical.sha256` | the exact digest of those bytes |
| `project.json` | a valid root descriptor |
| `consent-listed.json` | `mode: "listed"`, the mode `dollup init` writes |
| `consent-all.json` | `mode: "all"`, the blanket entry `--all` writes |
| `gsr-request.json` | a pending request, named by its identity hash |
| `gsr-decision.json` | a signed approval of that request |

The key behind `gsr-decision.json` is `SecretKey::generate([5u8; 32])` — a
test key, stated here so nobody wonders whether it is one.

`tests/fixtures.rs` asserts every one of these against the code. A fixture
that drifts from the implementation fails there rather than in whichever
repository notices second.
