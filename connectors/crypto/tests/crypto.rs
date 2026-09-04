//! The crypto connector: cap3's workload verb for verb, plus the refusals
//! and the forgeries that make the JWT surface worth having.

use std::sync::Arc;

use drt_caps::{CapSet, Grant, Scope};
use drt_connector::{Connector, Dispatcher, Registry};
use drt_connector_crypto::CryptoConnector;
use drt_hostcall::{to_bytes, Request, Status};

/// cap3's own key, from `cap3.host.lua`.
const DEV_KEY: &str = "capability-testing-dev-key-0123456789";

fn scope() -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("key".into(), DEV_KEY.into()),
        ("default_ttl".into(), rmpv::Value::from(3600u64)),
    ]))
}

fn args(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn call(
    c: &CryptoConnector,
    sc: &Scope,
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    pollster::block_on(c.call(name, Some(a), Some(sc))).map_err(|e| e.to_string())
}

fn field<'a>(v: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&rmpv::Value::Nil)
}

fn text(v: &rmpv::Value) -> &str {
    v.as_str().unwrap()
}

// ---------------------------------------------------------------------------
// cap3, as the supervisor runs it
// ---------------------------------------------------------------------------

#[test]
fn the_cap3_workload_runs() {
    let c = CryptoConnector::new();
    let sc = scope();

    // A digest and a keyed MAC over the same input.
    let hash = call(
        &c,
        &sc,
        "crypto/hash",
        args(vec![("data", "discofetch".into())]),
    )
    .unwrap();
    // SHA-256("discofetch"), which is a fact about SHA-256 and not about
    // this connector: if this line ever changes, the digest is wrong.
    assert_eq!(
        text(&hash),
        "544045ad8746c2e9b342129a0074e8df3ebe08677b5e72e23687e97d3329fc93"
    );

    let mac = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![("data", "discofetch".into())]),
    )
    .unwrap();
    assert_eq!(text(&mac).len(), 64);
    assert!(text(&mac).chars().all(|ch| ch.is_ascii_hexdigit()));
    // The MAC is not the hash: crypto/hmac is keyed.
    assert_ne!(text(&mac), text(&hash));

    // CSPRNG bytes, returned as hex — a nonce or an id is what a guest
    // wants, and it has no hex library of its own.
    let nonce = call(
        &c,
        &sc,
        "crypto/random",
        args(vec![("bytes", 16u64.into())]),
    )
    .unwrap();
    assert_eq!(text(&nonce).len(), 32);

    // JWT create + validate. jwt_sign owns iat/exp; the guest supplies claims.
    let token = call(
        &c,
        &sc,
        "crypto/jwt_sign",
        args(vec![
            (
                "claims",
                args(vec![("sub", "cap3".into()), ("role", "tester".into())]),
            ),
            ("ttl", 60u64.into()),
        ]),
    )
    .unwrap();
    let token = text(&token).to_string();

    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", token.as_str().into())]),
    )
    .unwrap();
    assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(true));
    assert_eq!(field(field(&v, "claims"), "sub").as_str(), Some("cap3"));
    assert_eq!(field(field(&v, "claims"), "role").as_str(), Some("tester"));

    // A tampered token must be rejected — proof verify is real and not a
    // rubber stamp. An invalid token is an *answer*, not an error.
    let tampered = format!("{}AAA", &token[..token.len() - 3]);
    let bad = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", tampered.as_str().into())]),
    )
    .unwrap();
    assert_eq!(field(&bad, "valid"), &rmpv::Value::Boolean(false));
    assert_eq!(field(&bad, "reason").as_str(), Some("signature"));
}

// ---------------------------------------------------------------------------
// The property the family exists for
// ---------------------------------------------------------------------------

/// Nothing in any reply carries the configured key, and nothing carries a
/// subkey either. This is the whole point: a guest holds the right to ask
/// for a signature, never the secret.
#[test]
fn no_reply_carries_the_key() {
    let c = CryptoConnector::new();
    let sc = scope();
    let replies = [
        call(&c, &sc, "crypto/hash", args(vec![("data", "x".into())])).unwrap(),
        call(&c, &sc, "crypto/hmac", args(vec![("data", "x".into())])).unwrap(),
        call(
            &c,
            &sc,
            "crypto/random",
            args(vec![("bytes", 32u64.into())]),
        )
        .unwrap(),
        call(
            &c,
            &sc,
            "crypto/jwt_sign",
            args(vec![("claims", args(vec![("sub", "x".into())]))]),
        )
        .unwrap(),
    ];
    for r in &replies {
        let rendered = format!("{r}");
        assert!(
            !rendered.contains(DEV_KEY),
            "a reply carried the key: {rendered}"
        );
    }
}

/// The `crypto/hmac` grant must not be usable as an oracle to forge a JWT.
/// The two grants sign under independently derived subkeys, so a program
/// holding only `host:crypto/hmac` cannot MAC a signing-input itself and
/// assemble a token.
#[test]
fn hmac_is_not_a_jwt_forging_oracle() {
    let c = CryptoConnector::new();
    let sc = scope();
    let token = call(
        &c,
        &sc,
        "crypto/jwt_sign",
        args(vec![("claims", args(vec![("sub", "victim".into())]))]),
    )
    .unwrap();
    let token = text(&token).to_string();
    let signing_input = &token[..token.rfind('.').unwrap()];

    // What an attacker with host:crypto/hmac can compute over exactly the
    // bytes a JWT signature covers.
    let oracle = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![("data", signing_input.into())]),
    )
    .unwrap();
    let oracle_bytes: Vec<u8> = text(&oracle)
        .as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    let forged = format!("{signing_input}.{}", base64_url_nopad(&oracle_bytes));
    assert_ne!(forged, token, "the hmac subkey produced the JWT signature");

    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", forged.as_str().into())]),
    )
    .unwrap();
    assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(false));
    assert_eq!(field(&v, "reason").as_str(), Some("signature"));
}

fn base64_url_nopad(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let cs = [n >> 18 & 63, n >> 12 & 63, n >> 6 & 63, n & 63];
        for (i, c) in cs.iter().enumerate() {
            if i <= chunk.len() {
                out.push(A[*c as usize] as char);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The JWT refusals, one per famous mistake
// ---------------------------------------------------------------------------

/// alg-confusion is closed structurally: the header segment is compared
/// against the one this host emits, never parsed. `alg:none` does not get
/// as far as a signature check.
#[test]
fn alg_none_is_refused_at_the_header() {
    let c = CryptoConnector::new();
    let sc = scope();
    // {"alg":"none","typ":"JWT"} . {"sub":"admin","exp":<far future>} . <empty>
    let header = base64_url_nopad(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = base64_url_nopad(br#"{"sub":"admin","exp":99999999999}"#);
    let token = format!("{header}.{payload}.");
    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", token.as_str().into())]),
    )
    .unwrap();
    assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(false));
    assert_eq!(field(&v, "reason").as_str(), Some("alg"));
}

/// The host owns iat/exp/nbf. A guest that sets them in its claims does not
/// get them: `exp` is the host's, computed from its clock and the ttl.
#[test]
fn a_guest_cannot_mint_a_forever_token() {
    let c = CryptoConnector::new();
    let sc = scope();
    let token = call(
        &c,
        &sc,
        "crypto/jwt_sign",
        args(vec![
            (
                "claims",
                args(vec![
                    ("sub", "sneaky".into()),
                    ("exp", 99_999_999_999u64.into()),
                    ("iat", 0u64.into()),
                    ("nbf", 0u64.into()),
                ]),
            ),
            ("ttl", 60u64.into()),
        ]),
    )
    .unwrap();
    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", text(&token).into())]),
    )
    .unwrap();
    let exp = field(field(&v, "claims"), "exp").as_i64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (now + 55..=now + 65).contains(&exp),
        "exp is {exp}, not the host's now+60"
    );
    // Exactly one exp claim survives — the guest's was dropped, not merged.
    let claims = field(&v, "claims").as_map().unwrap().to_vec();
    assert_eq!(
        claims
            .iter()
            .filter(|(k, _)| k.as_str() == Some("exp"))
            .count(),
        1
    );
}

/// The MAC is checked **before** anything is decoded or parsed, so the JSON
/// parser only ever runs on bytes this host signed. A well-formed token
/// with a bad signature stops at `signature` and never reaches the payload.
#[test]
fn the_mac_is_checked_before_the_payload_is_parsed() {
    let c = CryptoConnector::new();
    let sc = scope();
    // A payload that is not even JSON, behind a signature that is wrong.
    let token = format!(
        "{}.{}.{}",
        drt_connector_crypto::JWT_HEADER_B64,
        base64_url_nopad(b"not json at all"),
        base64_url_nopad(&[0u8; 32])
    );
    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", token.as_str().into())]),
    )
    .unwrap();
    assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(false));
    assert_eq!(field(&v, "reason").as_str(), Some("signature"));
}

/// The KDF is `dhost_crypto.c`'s, so a token minted by the C host verifies
/// here and vice versa. This test derives the JWT subkey the way the C does
/// — `HMAC-SHA256(master, "diluvium/crypto/jwt-hs256/v1")` — and signs a
/// payload the connector itself would never emit: one with no `exp`.
///
/// Two things fall out. The signature is accepted, which pins the subkey
/// derivation to the C's. And the token is still refused, because a signed
/// token without an integer `exp` has no enforceable expiry: treating it as
/// valid would make a missing or string-typed `exp` a forever-token.
#[test]
fn a_validly_signed_token_without_exp_is_still_refused() {
    let c = CryptoConnector::new();
    let sc = scope();
    let sign = |payload: &[u8]| {
        let k_jwt = hmac_sha256(DEV_KEY.as_bytes(), drt_connector_crypto::KDF_LABEL_JWT);
        let input = format!(
            "{}.{}",
            drt_connector_crypto::JWT_HEADER_B64,
            base64_url_nopad(payload)
        );
        format!(
            "{input}.{}",
            base64_url_nopad(&hmac_sha256(&k_jwt, input.as_bytes()))
        )
    };

    // The control: with an exp, the same construction verifies — so the
    // derivation matches and any refusal below is about the claim, not the
    // key.
    let far = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    let ok = sign(format!(r#"{{"sub":"foreign","exp":{far}}}"#).as_bytes());
    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", ok.as_str().into())]),
    )
    .unwrap();
    assert_eq!(
        field(&v, "valid"),
        &rmpv::Value::Boolean(true),
        "the KDF label no longer matches dhost_crypto.c"
    );

    for payload in [
        &br#"{"sub":"forever"}"#[..],
        &br#"{"sub":"forever","exp":"99999999999"}"#[..],
    ] {
        let token = sign(payload);
        let v = call(
            &c,
            &sc,
            "crypto/jwt_verify",
            args(vec![("token", token.as_str().into())]),
        )
        .unwrap();
        assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(false));
        assert_eq!(field(&v, "reason").as_str(), Some("expired"));
    }
}

/// HMAC-SHA256, the reference construction, so the test above derives the
/// subkey independently of the connector rather than asking it.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(msg);
    let mut outer = Vec::with_capacity(96);
    outer.extend(k.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

#[test]
fn an_expired_token_is_answered_not_errored() {
    let c = CryptoConnector::new();
    let sc = scope();
    // ttl of 1 second, then wait it out. now >= exp is the refusal.
    let token = call(
        &c,
        &sc,
        "crypto/jwt_sign",
        args(vec![
            ("claims", args(vec![("sub", "brief".into())])),
            ("ttl", 1u64.into()),
        ]),
    )
    .unwrap();
    let token = text(&token).to_string();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let v = call(
        &c,
        &sc,
        "crypto/jwt_verify",
        args(vec![("token", token.as_str().into())]),
    )
    .unwrap();
    assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(false));
    assert_eq!(field(&v, "reason").as_str(), Some("expired"));
}

#[test]
fn a_token_signed_under_another_key_does_not_verify() {
    let a = CryptoConnector::new();
    let b = CryptoConnector::new();
    let sc_a = scope();
    let sc_b = Scope(rmpv::Value::Map(vec![(
        "key".into(),
        "a-completely-different-dev-key-9876543210".into(),
    )]));
    let token = call(
        &a,
        &sc_a,
        "crypto/jwt_sign",
        args(vec![("claims", args(vec![("sub", "x".into())]))]),
    )
    .unwrap();
    let v = call(
        &b,
        &sc_b,
        "crypto/jwt_verify",
        args(vec![("token", text(&token).into())]),
    )
    .unwrap();
    assert_eq!(field(&v, "valid"), &rmpv::Value::Boolean(false));
    assert_eq!(field(&v, "reason").as_str(), Some("signature"));
}

#[test]
fn malformed_tokens_are_answers_too() {
    let c = CryptoConnector::new();
    let sc = scope();
    for (token, reason) in [
        ("", "malformed"),
        ("abc", "malformed"),
        ("a.b", "malformed"),
    ] {
        let v = call(
            &c,
            &sc,
            "crypto/jwt_verify",
            args(vec![("token", token.into())]),
        )
        .unwrap();
        assert_eq!(
            field(&v, "valid"),
            &rmpv::Value::Boolean(false),
            "{token:?}"
        );
        assert_eq!(field(&v, "reason").as_str(), Some(reason), "{token:?}");
    }
}

// ---------------------------------------------------------------------------
// hmac: named secrets and the constant-time verdict
// ---------------------------------------------------------------------------

#[test]
fn expect_turns_hmac_into_a_verdict() {
    let c = CryptoConnector::new();
    let sc = scope();
    let mac = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![("data", "payload".into())]),
    )
    .unwrap();
    let good = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![
            ("data", "payload".into()),
            ("expect", text(&mac).into()),
        ]),
    )
    .unwrap();
    assert_eq!(field(&good, "valid"), &rmpv::Value::Boolean(true));

    let mut wrong = text(&mac).to_string();
    wrong.replace_range(0..1, if wrong.starts_with('a') { "b" } else { "a" });
    let bad = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![
            ("data", "payload".into()),
            ("expect", wrong.as_str().into()),
        ]),
    )
    .unwrap();
    assert_eq!(field(&bad, "valid"), &rmpv::Value::Boolean(false));

    // A malformed expect is a caller error, not a false verdict: answering
    // {valid=false} to "these are not 64 hex digits" would hide a bug in
    // the caller behind a plausible-looking rejection.
    let err = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![
            ("data", "payload".into()),
            ("expect", "nothex".into()),
        ]),
    )
    .unwrap_err();
    assert!(err.contains("hex"), "{err}");
}

#[test]
fn a_named_secret_signs_with_its_own_bytes() {
    let c = CryptoConnector::new();
    let sc = Scope(rmpv::Value::Map(vec![
        ("key".into(), DEV_KEY.into()),
        (
            "secrets".into(),
            rmpv::Value::Array(vec![rmpv::Value::Map(vec![
                ("name".into(), "github".into()),
                ("key".into(), "the-webhook-shared-secret-01234".into()),
            ])]),
        ),
    ]));
    let derived = call(&c, &sc, "crypto/hmac", args(vec![("data", "body".into())])).unwrap();
    let named = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![("data", "body".into()), ("key", "github".into())]),
    )
    .unwrap();
    assert_ne!(text(&derived), text(&named));

    // HMAC-SHA256("the-webhook-shared-secret-01234", "body") — the peer's
    // own computation, which is the entire point of the raw path.
    assert_eq!(
        text(&named),
        "d60142f66b7612fef7070c65354d39a775b5b767f6ef4d4eafce4242ba586bf6"
    );

    // A name this deployment does not configure is refused, never quietly
    // signed under the default key.
    let err = call(
        &c,
        &sc,
        "crypto/hmac",
        args(vec![("data", "body".into()), ("key", "gitlab".into())]),
    )
    .unwrap_err();
    assert!(err.contains("no secret named 'gitlab'"), "{err}");
}

// ---------------------------------------------------------------------------
// turn_credential
// ---------------------------------------------------------------------------

#[test]
fn turn_is_refused_until_it_is_configured() {
    let c = CryptoConnector::new();
    let err = call(
        &c,
        &scope(),
        "crypto/turn_credential",
        args(vec![("user", "alice".into())]),
    )
    .unwrap_err();
    assert!(err.contains("no TURN shared secret"), "{err}");
}

#[test]
fn turn_credential_is_coturns_scheme() {
    let c = CryptoConnector::new();
    let sc = Scope(rmpv::Value::Map(vec![
        ("key".into(), DEV_KEY.into()),
        (
            "turn".into(),
            rmpv::Value::Map(vec![
                ("key".into(), "the-coturn-static-auth-secret-1".into()),
                ("ttl".into(), rmpv::Value::from(600u64)),
                (
                    "uris".into(),
                    rmpv::Value::Array(vec!["turn:turn.example:3478".into()]),
                ),
            ]),
        ),
    ]));
    let v = call(
        &c,
        &sc,
        "crypto/turn_credential",
        args(vec![("user", "alice".into())]),
    )
    .unwrap();
    let username = text(field(&v, "username")).to_string();
    let expires = field(&v, "expires").as_i64().unwrap();
    // "<expiry-unix>:<user>", and the expiry in it is the host's, not the
    // guest's: the call takes a ttl and never a timestamp.
    assert_eq!(username, format!("{expires}:alice"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(
        (now + 595..=now + 605).contains(&expires),
        "expires is {expires}"
    );
    // The password is standard base64 (not base64url, and padded) of
    // HMAC-SHA1(shared_secret, username) — coturn recomputes exactly this
    // from the same secret, so the assembly is the thing worth pinning.
    let password = text(field(&v, "password"));
    let mut mac = <hmac::SimpleHmac<sha1::Sha1> as hmac::Mac>::new_from_slice(
        b"the-coturn-static-auth-secret-1",
    )
    .unwrap();
    hmac::Mac::update(&mut mac, username.as_bytes());
    let want = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        hmac::Mac::finalize(mac).into_bytes(),
    );
    assert_eq!(password, want);
    assert_eq!(password.len(), 28);
    assert!(password.ends_with('='));
    assert_eq!(
        field(&v, "uris"),
        &rmpv::Value::Array(vec!["turn:turn.example:3478".into()])
    );
}

// ---------------------------------------------------------------------------
// Wiring: the scope fails at startup, and the grants gate the calls
// ---------------------------------------------------------------------------

#[test]
fn a_short_or_missing_key_fails_at_wiring_time() {
    for (scope, expect) in [
        (None, "signing key"),
        (
            Some(Scope(rmpv::Value::Map(vec![(
                "key".into(),
                "tooshort".into(),
            )]))),
            "16 bytes",
        ),
        (
            Some(Scope(rmpv::Value::Map(vec![(
                "key_env".into(),
                "DRT_CRYPTO_KEY_THAT_IS_NOT_SET".into(),
            )]))),
            "is not set",
        ),
    ] {
        let mut reg = Registry::new();
        let err = reg
            .wire("crypto", Arc::new(CryptoConnector::new()), scope)
            .unwrap_err()
            .to_string();
        assert!(err.contains(expect), "{err}");
        assert!(err.contains("host:crypto"), "{err}");
    }
}

#[test]
fn jwt_sign_is_a_separate_grant_from_jwt_verify() {
    let mut reg = Registry::new();
    reg.wire("crypto", Arc::new(CryptoConnector::new()), Some(scope()))
        .unwrap();
    let d = Dispatcher::new(reg);
    // A verifier: it may check tokens and hash, and may not mint.
    let caps = CapSet::root(vec![
        Grant::grant("host:crypto/jwt_verify"),
        Grant::grant("host:crypto/hash"),
    ]);
    let sign = to_bytes(&Request {
        tok: 1,
        call: "crypto/jwt_sign".into(),
        args: Some(args(vec![("claims", args(vec![("sub", "x".into())]))])),
    })
    .unwrap();
    let reply = pollster::block_on(d.dispatch(&caps, &sign));
    assert_eq!(reply.status, Status::Denied);

    let hash = to_bytes(&Request {
        tok: 2,
        call: "crypto/hash".into(),
        args: Some(args(vec![("data", "x".into())])),
    })
    .unwrap();
    assert_eq!(
        pollster::block_on(d.dispatch(&caps, &hash)).status,
        Status::Ok
    );
}

#[test]
fn an_unknown_call_in_the_family_is_an_error_not_a_panic() {
    let c = CryptoConnector::new();
    let err = call(&c, &scope(), "crypto/sign_anything", args(vec![])).unwrap_err();
    assert!(err.contains("no call 'crypto/sign_anything'"), "{err}");
}

/// `crypto/random` is a *baseline* family (doc/HostBaseline.md): every DRT
/// host must answer it, and the Lab's JavaScript host answers it with the
/// same argument, the same default and the same range. So the numbers below
/// are a cross-host contract rather than this connector's preference --
/// changing one means changing two hosts, and rule 5 there is why there is
/// no second spelling of it to change instead.
#[test]
fn random_bounds_are_refusals() {
    let c = CryptoConnector::new();
    let sc = scope();
    for n in [0i64, -1, 1025] {
        let err = call(&c, &sc, "crypto/random", args(vec![("bytes", n.into())])).unwrap_err();
        assert!(err.contains("1..1024"), "{err}");
    }
    // The default, when the guest names no size.
    let d = call(&c, &sc, "crypto/random", rmpv::Value::Map(vec![])).unwrap();
    assert_eq!(text(&d).len(), 64);
    // Two calls do not agree — it is a CSPRNG, not a counter.
    let a = call(
        &c,
        &sc,
        "crypto/random",
        args(vec![("bytes", 32u64.into())]),
    )
    .unwrap();
    let b = call(
        &c,
        &sc,
        "crypto/random",
        args(vec![("bytes", 32u64.into())]),
    )
    .unwrap();
    assert_ne!(text(&a), text(&b));
}
