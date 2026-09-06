# `drt turn` answers 400 where RFC 8489 says 401

**Audience: whoever files this against `webrtc-rs/webrtc` (the `turn`
crate).** This is a DRT document about someone else's bug, written here
because DRT is where it was measured and DRT is what it breaks. Nothing in
it depends on DRT — the reproduction is four lines of `turn::client`.

**Status: open.** `turn` 0.17.2 is the newest release as of 2026-09-06 and
carries the bug. Reported to DRT as issue #17's first nit, by a hand-built
Allocate against rc4; confirmed here against the crate's own client and
against its source.

---

## The bug in one paragraph

`Request::authenticate_request` builds **one** `bad_request_msg`
(`ErrorCodeAttribute { code: CODE_BAD_REQUEST }`) at the top and sends it
down every authentication-failure path, including the two that RFC 8489
§9.2.4 says must be answered **401 Unauthorized with a fresh NONCE**: an
unknown username (`auth_handler.auth_handle` returns `Err`) and a
MESSAGE-INTEGRITY that does not verify (`mi.check` returns `Err`). The
challenge path is correct — a request with no MESSAGE-INTEGRITY at all
gets `respond_with_nonce(.., CODE_UNAUTHORIZED)` — so a server looks right
until a credential is actually wrong.

`turn-0.17.2/src/server/request.rs`, in `authenticate_request`:

- the shared 400 is built at the top of the function;
- `Err(_) => build_and_send_err(.., bad_request_msg, Error::ErrNoSuchUser)`
  — unknown user;
- `if let Err(err) = mi.check(..) { build_and_send_err(.., bad_request_msg, ..) }`
  — bad integrity.

Both should be `self.respond_with_nonce(m, calling_method, CODE_UNAUTHORIZED)`,
which the same function already calls for the no-integrity case and which
already handles minting and registering the NONCE.

## Why it matters rather than being a spec nit

401 means *authenticate again*; 400 means *your request is malformed*. A
browser's ICE agent acts on that difference: it retries a 401 with
credentials and treats a 400 as terminal. So the case this breaks is not a
typo'd password, which is unrecoverable anyway — it is a **credential that
expired mid-session**.

DRT mints `<expiry>:<principal>` credentials under coturn's
`use-auth-secret` scheme (`doc/WireGuard.md`, and DRT issue #12). When one
lapses, the client's next refresh is answered 400 and the allocation dies
with it, where a 401 would have let the client re-authenticate. That
compounds with the refresh behaviour tracked in DRT issue #17 §3: the TURN
client refreshes with the same credential it allocated with, so expiry is
reached in normal operation rather than only when something is
misconfigured.

## Reproduction

Any `turn` server built with an auth handler that rejects unknown users:

```rust
let (username, password) = /* a credential the server will not verify */;
let client = Client::new(ClientConfig { username, password, .. }).await?;
let err = client.allocate().await.unwrap_err();
// today:    "Allocate error response (error 400: )"
// expected: 401, with a fresh NONCE and REALM to retry against
```

Measured in DRT as
`crates/drt/tests/turn.rs::a_forged_credential_is_refused_with_the_wrong_code_for_now`,
which asserts the **400** deliberately. That test failing is the signal
that upstream has fixed this: change it to 401 and delete this document.

## What DRT can and cannot do about it

Nothing, from outside the crate. The error code is chosen inside
`authenticate_request`, and the only hook DRT has is the `AuthHandler`,
whose contract is `Result<Vec<u8>, Error>` — every `Err` becomes the same
400, and returning `Ok` with a wrong key just moves the failure to the
integrity check, which is also 400. Rewriting the code on the wire would
mean minting a NONCE the crate manages internally.

So the options are: land the fix upstream; carry a `[patch.crates-io]`
fork of `turn` until it lands; or document it, which is what this is. DRT
holds the third, because a fork of a protocol crate on the authentication
path is a decision to take deliberately rather than as a drive-by — the
same reasoning that put the TURN server on a maintained crate instead of
in this tree.
