# DRT's reply to discofetch's asks

**Written 2026-09-06, against the v0.5.0rc4 branch (`claude/v0-5-0rc2-release-p9v3a8`).**
Answers `discofetch:doc/DRT_ASKS.md` as it stood on
`claude/drt-discofetch-integration-ioqi4x`, verified by them against
v0.4.2 and v0.5.0rc3 on 6 Sep.

Same convention as `doc/Ask-0.5.0-Reply.md` and as the asks themselves:
`landed` names the commit and the test; `open` says what it would take and
where it stands; a correction is kept in place rather than quietly folded
in. An ask that shipped with a measurement gets a measurement back.

**Short version.** The four near-term asks that are `netcheck`'s and
`buildinfo`'s (§1, §2, §3, §7) landed in rc4, with tests. TURN (§4)
landed after it, the day issue #12 decided the credential shape. The
punch inside the tunnel (§8) is open in the way its own text says: a
spec to follow, not a dependency. One thing the
asks did not know to ask for is in rc4 anyway and matters more to a
fetchpoint than any of them: `rest` stalled every instance for the
length of a call in every candidate before it.

---

## surface block

1. Landed, with the test that holds each.
2. Open, sized.
3. A correction to something the asks assume.
4. What was not asked and shipped anyway.

---

## 1. Landed

### §1 `netcheck --json` omits the evidence — `landed`

`render_json` mirrors every line of `render_text` as structured pairs
under `evidence`: `address {ip, cgnat, why}`, `v6`, `udp {mapping, ports:
[{server, port}], why}`, `tcp {views: [{edge, port, dest}],
same_source_port, agrees, stable, why}`, `inbound {results: [{port,
result}], why}`. "Not measured" is `null`, and the `why` beside it is the
text's parenthetical. serde_json builds it, so a `why` carrying a quote is
no longer turned into an apostrophe.

Under `"schema": 1`, which is your "later, tracked" ask for a version the
punch tool can refuse, answered while the shape is new rather than after
it has consumers.

The spec's rule that an absent port is *never zero* is kept on the wire: an
`EdgeView` without `x-real-port` is `"port": null`, and the test asserts
it. `crates/drt/src/netcheck.rs`,
`the_json_form_carries_the_evidence_as_structured_pairs`.

### §2 `netcheck --udp-port N` — `landed`

Threaded to `ProbeConfig::bind_addr` with the wildcard address kept, so
only the port changes. A port that cannot be bound is a refusal by name
under `udp map` — `--udp-port 51820: <the OS's reason>` — and never a
silent fall back to ephemeral, which would be the same wrong answer with a
confident face. The measurement it makes is the one `punch-wg.sh` needs:
the mapping *of the flow WireGuard will use*.

### §3 `buildinfo` should say the tag — `landed`

`build.rs` re-exports `DRT_RELEASE_TAG` from the build environment;
`release.yml` hands the preflight's tag to all three build jobs; `buildinfo`
prints `tag: v0.5.0rc4` and `"tag":"v0.5.0rc4"`, or no line and `null`
for a local build. The native smoke step asserts the binary says it back
on a tagged build and says nothing on a rehearsal, so the first release to
carry this is the first to check it. `/usr/local/lib/drt.pin` can go.

### §7 stale help text — `landed`

All three, and the lesson taken rather than the sentences patched:
`--reflect-at`'s text now says why it stays necessary (one `--reflect` is
one vantage whatever its name resolves to) rather than until which record
lands; `--port` is no longer "experimental" with a server "not deployed
yet", and its text says why a deployment fact does not belong in help
text; the STUN pair is named by zone, since that is what a user types.

### §5, §6 (cross-references to vera §20, §23) — `landed`, half

`Date` on `ssmtp/send` and a returned `Message-ID`: landed, see
`doc/Ask-Vera-Reply.md` §20. `extra_roots` on the `rest` scope: landed —
the interception CA is added beside webpki's roots, never instead.
Proxy traversal (CONNECT) is open; `netcheck`'s reflect fetch goes through
the same TLS builder, so it gains `extra_roots` the day `netcheck` takes
a scope for it, which it does not yet.

---

## 2. Open

### §4 TURN — `landed`, once issue #12 decided the shape

Confirmed as described, and then decided rather than deferred: issue #12
fixed the credential -- an optional principal, coturn's
`<expiry>:<principal>`, HMAC-SHA1 under the shared secret, opaque to the
server -- and ego-transport at the pin already verified both forms
(`ephemeral_credentials_for`, `credential_principal`,
`revoke_principal`), so the half that was this repository's could land
the same day: a `turn` block beside `stun`, `drt turn` in the foreground,
and inside `drt start` the counters on a timer and **every allocation's
closing byte count with its principal, as it happens** -- the
`turn_closed` message, which is the per-principal close accounting the
later-tracked list asked for. The secret is `connectors.crypto.turn`'s
three knobs, because it is the same secret, and a block with none refuses
to bind. `crates/drt/tests/turn.rs` allocates with webrtc-rs's client
rather than ours, and with the connector's own credential against the
deployment's own relay, which is the two halves measured against each
other. What `api.host.lua` gains is the line the issue wrote: the same
secret under `turn`. The bitrate cap stays open.

### §8 `drt tunnel --direct` — `open`, as filed

Every ingredient but two exists, exactly as the ask counts them, and the
two are the large ones: an ICE-lite and a reliable stream over the UDP
hole. QUIC via quinn is the sane route since rustls is already there, and
nothing about it is small. Taken at the ask's own word — a spec to
follow, not a dependency — and nothing here is planned around it.

One thing found while writing `doc/Python.md` that belongs beside this,
because it is the same story one layer up: **`ssh/exec` and the relay did
not compose.** The scope dials a `host:port`; the tunnel's caller half
was stdio-only, the ProxyCommand shape; `--listen` is the *other* half.
So a deployment could not reach a parked device's sshd through the relay
from inside a program, only from a shell. Filed by you as issue #13 with
the shape spelled out, and `landed` as filed: `drt tunnel <url> --local
HOST:PORT` binds a local listener where each accepted connection claims
one fresh leg, N connections are N legs with nothing multiplexed, and a
claim the relay refuses closes the local connection at once — the 403
and the missing-relay cases both, measured in
`crates/drt/tests/tunnel.rs`. `--local` and not `--listen`, for the
reason the issue gives. It is the piece a punch inside the tunnel would
need closed first, and it is closed.

### Later, tracked

- **`--json` schema version** — landed with §1.
- **Per-principal close accounting on TURN** — landed with §4, as the
  `turn_closed` message. **A bitrate cap** — open, behind it.

---

## 3. A correction

The asks were verified against rc3 and assume rc3's runtime is the one
rc1's notes describe: a slow connector is parked and the swarm keeps
stepping. **It was not.** `run`, `repl` and `start` entered no tokio
runtime, so every tokio-backed connector took its `block_on` fallback on
the drive thread; measured, a child's three-second `rest/get` held its
parent's next hostcall for 3004 ms on rc3. rc4 enters the runtime and the
same measurement reads 1 ms, with the call still completing. A fetchpoint
whose `rest` calls were sized around "the process stops for this" is
measuring a different runtime from rc4 on. Details: `CHANGELOG.yaml`
0.5.0 Fixed, `crates/drt/src/runtime.rs`.

---

## 4. Not asked, shipped anyway

- `capabilities/list`, the menu (vera §14), which a deployment script can
  use to check a config wired what it thinks it wired.
- Regular expressions in the language, from diluvium build13.
- `admit_timeout_ms` on listeners (rc3): a request arriving before the
  program declared its queue waits instead of being refused, which is the
  single-shot-handshake case a `mod_rest`-shaped caller cannot survive.
