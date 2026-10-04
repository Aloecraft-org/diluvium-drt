# TURN as a last-resort candidate for `drt p2p` (design note, for decision)

Status: accepted 2026-10-04 ("Build it"), and built as below, with one change: the relayed line travels in a new record key `r`, not in `c`. The shared vectors pin that a reader drops a relay line from `c`, so `c` stays as it was and `r` is a key a page's reader ignores. Evidence is cited by
`file:line` in diluvium-drt `main` at 447e776.

## The problem

Two native peers that are both behind a symmetric NAT or CGNAT have no
direct path. The call fails and says so (`doc/P2P.md` §1), unless the
caller names `--relay` or `--fallback`. Both of those need a DRT peer
somewhere that both sides can reach, running `--forward` as a relay.
A plain TURN server, either `drt turn` or coturn, is the standard tool for
this, and DRT already ships one (`crates/drt/src/turn.rs`). p2p cannot
use it.

## What stands in the way today

- **The record refuses relay lines.**
  - `doc/BrowserAccess.md:111` says: "No `relay`: there is no TURN in v1."
  - `usable_candidate` keeps only host, srflx and prflx
    (`crates/drt-rtc/src/record.rs:188-213`).
  - Sessions drop relayed candidates a second time (`host.rs:1186`,
    `caller.rs:299`).
- **There is no TURN client on the p2p socket.** drt-rtc is str0m, which
  is sans-IO with one UDP socket for every session. str0m already accepts
  a relayed local candidate (`Candidate::relayed`). Its docs say the
  application allocates the address and str0m pairs it like any other
  candidate.
- **Owner decision, 2026-09-24:** "Browsers get no TURN" (`doc/Plan-0.8.0.md:29`).

## Proposal

1. **Native only.** The page keeps its filter (`drt_browser_access.js:202`,
   `:211-243`), as the 2026-09-24 decision says. Nothing in the browser
   changes.
2. **The record stays `v: 1`.** A side with TURN publishes one
   `typ relay` line among its candidates.
   - This needs no version bump. A v1 reader already drops a relay line
     "on the way in" (`BrowserAccess.md:123-125`), and the record's rule
     is that only a change a v1 reader cannot ignore needs `v: 2`
     (`:97-98`).
   - An older peer just never uses the line.
   - A relay line is about 60 bytes, so it fits the 512-byte and
     8-candidate caps.
   - Native readers accept `relay`; the page's reader still drops it.
   - §2.1 is amended to say so.
3. **Only one side needs TURN.** If A publishes a relayed address, B sends
   checks to it from its ordinary socket. A's allocation permits B by IP,
   which A has from B's srflx line. TURN permissions are per IP, so B's
   symmetric NAT changing ports does not matter. B needs a build that
   reads relay lines, but no TURN server of its own.
4. **TURN is used only when a flag asks for it.** This keeps the P2P.md
   rule that "TURN and relays are in the path… neither is used without a
   flag" (`doc/P2P.md:45`).
   - The flag is `--turn turn://<user>:<pass>@host[:port]`, repeatable,
     with the config key `p2p.turn`. Like `headers`, it belongs in a 0600
     config file rather than on `ps`.
   - It works for call, park and listen.
   - "Last resort" comes from ICE priority. A relay candidate ranks below
     host and srflx, so str0m picks it only when nothing direct
     succeeds. The flag gives permission to use TURN; ICE decides whether
     it is needed.
5. **The client is the `turn` crate DRT already builds.**
   - WireGuard's TURN fallback already uses it (`wireguard.rs:2288-2327`).
   - `wireguard`, which is in `full`, already pulls in `turn-client`
     (`crates/drt/Cargo.toml` features), so `full` gains nothing.
   - On `slim,p2p`, the Windows row, the TURN path is gated behind
     `turn-client` and that row stays as it is.
   - The allocation gets its own socket, as in WireGuard.
   - str0m's transmits from the relayed address go to the allocation's
     `send_to`. Data that arrives through the allocation is handed to
     str0m as received on the relayed address.
   - A host keeps one allocation for all its sessions and adds a
     permission per peer.
6. **The failure note learns about TURN.** A `relay` verdict names
   `--turn` beside `--fallback` and `--relay`. A call that had `--turn`
   and still failed says that the relay was tried.

## Not in this proposal

- **Server-minted credentials.** The signalling server would hand out a
  `crypto/turn_credential` result with the answer: the "`--ice` answer"
  that `turn.rs:25` mentions, which is not defined anywhere. That is a
  DRT-Signalling extension and a separate decision. The static `--turn`
  credential comes first.
- **TURN over TCP or TLS** (port 443 for networks that block UDP). The
  `wss://` park already covers that case.
- **Browsers.** This stays out until you revisit the 2026-09-24 decision.

## Cost and test

- **Code**
  - One module in drt-rtc, or in `drt/src/p2p`, to bridge allocation and
    str0m.
  - The record filter.
  - The flag and config key.
  - Docs: `doc/P2P.md` §1 and the transport matrix, plus
    `BrowserAccess.md` §2.1.
- **Test.** An integration test runs `drt turn` on loopback, plus a caller
  and an answerer whose srflx and host candidates are made unusable.
  - A test-only switch publishes only relay candidates, since loopback
    cannot fake a symmetric NAT.
  - The test asserts that the session comes up through the allocation
    and that `turn_closed` counts the bytes.
- **Kill threshold.** If str0m's relayed-candidate path cannot be driven
  from a separate socket without patching str0m, stop. The answer then
  stays `--fallback`.

## Decisions for you

1. Native only, browsers unchanged: **yes** (recommended) or revisit the
   browser decision now.
2. Record stays v1 with relay lines allowed: **yes** (recommended) or bump
   to v2.
3. Credentials from a static `--turn` URI first, server-minted later:
   **yes** (recommended) or design server-minted credentials first.
