//! `crypto/jwt_verify {token, jwks}` and the `{"verify_only": true}` scope:
//! a service that holds only an issuer's public keys checks that issuer's
//! tokens, and nothing in the token, the key set or the host's own keys
//! changes which key does the checking.
//!
//! The issuer here is a second connector with a configured signing key,
//! the way the session framework and the signalling server are two
//! processes. Forged tokens are signed with `ed25519_dalek` directly, so a
//! test that expects `signature` or `expired` is refused for that reason
//! and not an earlier one.

use base64::Engine as _;
use drt_caps::{Scope, ScopeType as _};
use drt_connector::{Asker, Caller, Connector};
use drt_connector_crypto::CryptoConnector;
use ed25519_dalek::Signer as _;

const B64URL: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;
const MASTER: &str = "capability-testing-dev-key-0123456789";

// depth: helpers

fn map(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

/// A PKCS#8 PEM for a raw Ed25519 secret (RFC 8410's fixed prefix).
fn pem(secret: [u8; 32]) -> String {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&secret);
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n")
}

/// An issuer's scope: a master and one configured EdDSA key, `name`.
fn issuer_scope(name: &str, secret: [u8; 32]) -> Scope {
    Scope(map(vec![
        ("key", MASTER.into()),
        (
            "signing_keys",
            rmpv::Value::Array(vec![map(vec![
                ("name", name.into()),
                ("alg", "EdDSA".into()),
                ("private_key", pem(secret).into()),
            ])]),
        ),
    ]))
}

fn verify_only_scope() -> Scope {
    Scope(map(vec![("verify_only", true.into())]))
}

fn call(sc: &Scope, name: &str, a: rmpv::Value) -> Result<rmpv::Value, String> {
    let c = CryptoConnector::new();
    let asker = Asker {
        caller: Caller::Node(1),
        grants: &[],
    };
    pollster::block_on(c.call_as(&asker, name, Some(a), Some(sc))).map_err(|e| e.to_string())
}

fn sign(sc: &Scope, key: &str, claims: rmpv::Value) -> String {
    call(
        sc,
        "crypto/jwt_sign",
        map(vec![
            ("claims", claims),
            ("ttl", 600.into()),
            ("key", key.into()),
        ]),
    )
    .unwrap()
    .as_str()
    .unwrap()
    .to_string()
}

fn jwk(sc: &Scope, key: &str) -> rmpv::Value {
    call(sc, "crypto/public_key", map(vec![("key", key.into())])).unwrap()
}

fn jwks(keys: Vec<rmpv::Value>) -> rmpv::Value {
    map(vec![("keys", rmpv::Value::Array(keys))])
}

fn verify(sc: &Scope, token: &str, set: rmpv::Value) -> Result<rmpv::Value, String> {
    call(
        sc,
        "crypto/jwt_verify",
        map(vec![("token", token.into()), ("jwks", set)]),
    )
}

fn field<'a>(v: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&rmpv::Value::Nil)
}

fn reason(v: &rmpv::Value) -> &str {
    field(v, "reason").as_str().unwrap_or("")
}

fn valid(v: &rmpv::Value) -> bool {
    field(v, "valid") == &rmpv::Value::Boolean(true)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A token signed by hand under `secret`, with `header` and `payload` as
/// written: what a forger, or another library, would send.
fn forge(secret: [u8; 32], header: &str, payload: &str) -> String {
    let input = format!(
        "{}.{}",
        B64URL.encode(header.as_bytes()),
        B64URL.encode(payload.as_bytes())
    );
    let sig = ed25519_dalek::SigningKey::from_bytes(&secret).sign(input.as_bytes());
    format!("{input}.{}", B64URL.encode(sig.to_bytes()))
}

const ISSUER: [u8; 32] = [7; 32];
const OTHER: [u8; 32] = [9; 32];

// depth: the tests

/// The session issuer signs; a verify_only connector, given the issuer's
/// JWK as a table or as the JSON text a kv value holds, answers the claims.
#[test]
fn a_verifier_with_only_the_jwks_checks_the_issuers_token() {
    let issuer = issuer_scope("session-1", ISSUER);
    let groups = rmpv::Value::Array(vec!["staff".into(), "admins".into()]);
    let token = sign(
        &issuer,
        "session-1",
        map(vec![("sub", "alice".into()), ("groups", groups.clone())]),
    );
    let set = jwks(vec![jwk(&issuer, "session-1")]);
    let v = verify(&verify_only_scope(), &token, set.clone()).unwrap();
    assert!(valid(&v), "{v:?}");
    let claims = field(&v, "claims");
    assert_eq!(field(claims, "sub").as_str(), Some("alice"));
    assert_eq!(field(claims, "groups"), &groups);

    // The same set as JSON text, as `GET /v1/<topic>/kv/jwks` returns it.
    let text = serde_json::json!({"keys": [{
        "kty": "OKP", "crv": "Ed25519", "kid": "session-1", "alg": "EdDSA", "use": "sig",
        "x": field(&set.as_map().unwrap()[0].1.as_array().unwrap()[0], "x").as_str().unwrap(),
    }]})
    .to_string();
    let v = verify(&verify_only_scope(), &token, text.as_str().into()).unwrap();
    assert!(valid(&v), "{v:?}");

    // A connector with a key of its own takes the jwks path too.
    let v = verify(&issuer_scope("mine", OTHER), &token, set).unwrap();
    assert!(valid(&v), "{v:?}");
}

/// With `jwks` named, the host's own keys are not consulted: a token the
/// host itself signed does not verify against a set that lacks its key.
#[test]
fn with_jwks_named_the_hosts_own_keys_do_not_count() {
    let host = issuer_scope("session-1", ISSUER);
    let token = sign(&host, "session-1", map(vec![("sub", "alice".into())]));
    let elsewhere = issuer_scope("session-2", OTHER);
    let v = verify(&host, &token, jwks(vec![jwk(&elsewhere, "session-2")])).unwrap();
    assert_eq!(reason(&v), "kid");
}

/// A key under the same `kid` but another `x` is a signature failure, and
/// a swapped payload is too; neither reaches the claims.
#[test]
fn another_key_under_the_same_kid_and_a_swapped_payload_fail_the_signature() {
    let issuer = issuer_scope("session-1", ISSUER);
    let token = sign(&issuer, "session-1", map(vec![("sub", "alice".into())]));
    let impostor = issuer_scope("session-1", OTHER);
    let v = verify(
        &verify_only_scope(),
        &token,
        jwks(vec![jwk(&impostor, "session-1")]),
    )
    .unwrap();
    assert_eq!(reason(&v), "signature");

    let set = jwks(vec![jwk(&issuer, "session-1")]);
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    parts[1] = B64URL.encode(format!(r#"{{"sub":"root","exp":{}}}"#, now() + 600));
    let v = verify(&verify_only_scope(), &parts.join("."), set).unwrap();
    assert_eq!(reason(&v), "signature");
}

/// The header is rebuilt from the key, never read for `alg`: an HS256 or
/// `none` header naming the kid, a header with its members reordered (what
/// some libraries write), and a header naming no kid all fail before the
/// signature is looked at.
#[test]
fn the_header_is_rebuilt_from_the_key_and_alg_is_never_read() {
    let issuer = issuer_scope("session-1", ISSUER);
    let set = || jwks(vec![jwk(&issuer, "session-1")]);
    let payload = format!(r#"{{"sub":"mallory","exp":{}}}"#, now() + 600);
    for (header, why) in [
        (r#"{"alg":"HS256","typ":"JWT","kid":"session-1"}"#, "alg"),
        (r#"{"alg":"none","typ":"JWT","kid":"session-1"}"#, "alg"),
        (r#"{"typ":"JWT","alg":"EdDSA","kid":"session-1"}"#, "alg"),
        (r#"{"alg":"EdDSA","typ":"JWT"}"#, "kid"),
    ] {
        let v = verify(
            &verify_only_scope(),
            &forge(ISSUER, header, &payload),
            set(),
        )
        .unwrap();
        assert_eq!(reason(&v), why, "{header}");
    }
    // The exact header, signed by the issuer's secret, does verify: the
    // refusals above were for the header, not the signature.
    let ok = forge(
        ISSUER,
        r#"{"alg":"EdDSA","typ":"JWT","kid":"session-1"}"#,
        &payload,
    );
    assert!(valid(&verify(&verify_only_scope(), &ok, set()).unwrap()));
}

/// The host owns the time: an expired token and one with no integer `exp`
/// are refused with a good signature, and `nbf` in the future waits.
#[test]
fn expiry_is_required_and_checked_against_the_host_clock() {
    let issuer = issuer_scope("session-1", ISSUER);
    let set = || jwks(vec![jwk(&issuer, "session-1")]);
    let header = r#"{"alg":"EdDSA","typ":"JWT","kid":"session-1"}"#;
    let t = now();
    for (payload, why) in [
        (format!(r#"{{"sub":"a","exp":{}}}"#, t - 1), "expired"),
        (r#"{"sub":"a"}"#.to_string(), "expired"),
        (format!(r#"{{"sub":"a","exp":"{}"}}"#, t + 600), "expired"),
        (
            format!(r#"{{"sub":"a","exp":{},"nbf":{}}}"#, t + 600, t + 300),
            "not_yet_valid",
        ),
    ] {
        let v = verify(
            &verify_only_scope(),
            &forge(ISSUER, header, &payload),
            set(),
        )
        .unwrap();
        assert_eq!(reason(&v), why, "{payload}");
    }
}

/// Only an Ed25519 OKP key whose `alg` and `use`, if given, are EdDSA and
/// sig, with a canonical `x` that is a point of large order, is usable.
/// Every other entry is skipped, and a kid two usable keys share matches
/// neither.
#[test]
fn only_usable_ed25519_keys_count_and_a_shared_kid_matches_nothing() {
    let issuer = issuer_scope("session-1", ISSUER);
    let token = sign(&issuer, "session-1", map(vec![("sub", "alice".into())]));
    let good = jwk(&issuer, "session-1");
    let x = field(&good, "x").as_str().unwrap().to_string();
    let with = |changes: Vec<(&str, rmpv::Value)>| {
        let mut entries: Vec<(rmpv::Value, rmpv::Value)> = good.as_map().unwrap().clone();
        for (k, v) in changes {
            entries.retain(|(name, _)| name.as_str() != Some(k));
            if !v.is_nil() {
                entries.push((k.into(), v));
            }
        }
        rmpv::Value::Map(entries)
    };
    // The identity point: valid encoding, small order.
    let mut identity = [0u8; 32];
    identity[0] = 1;
    for (bad, what) in [
        (with(vec![("kty", "RSA".into())]), "kty"),
        (with(vec![("crv", "X25519".into())]), "crv"),
        (with(vec![("alg", "RS256".into())]), "alg"),
        (with(vec![("use", "enc".into())]), "use"),
        (with(vec![("x", format!("{x}=").into())]), "padded x"),
        (
            with(vec![("x", B64URL.encode([1u8; 31]).into())]),
            "short x",
        ),
        (
            with(vec![("x", B64URL.encode(identity).into())]),
            "small order",
        ),
        (with(vec![("kid", rmpv::Value::Nil)]), "no kid"),
    ] {
        let v = verify(&verify_only_scope(), &token, jwks(vec![bad])).unwrap();
        assert_eq!(reason(&v), "kid", "{what}");
    }
    // Without alg and use, the key is still usable; beside a skipped RSA
    // key it is found.
    let bare = with(vec![("alg", rmpv::Value::Nil), ("use", rmpv::Value::Nil)]);
    let rsa = map(vec![("kty", "RSA".into()), ("kid", "session-1".into())]);
    assert!(valid(
        &verify(&verify_only_scope(), &token, jwks(vec![rsa, bare])).unwrap()
    ));
    // Two usable keys under one kid: neither is taken.
    let v = verify(
        &verify_only_scope(),
        &token,
        jwks(vec![good.clone(), good.clone()]),
    )
    .unwrap();
    assert_eq!(reason(&v), "kid");
}

/// A `jwks` that is not a key set is the program's mistake, a call error,
/// and so is a set past the limits.
#[test]
fn a_jwks_that_is_not_a_key_set_is_a_call_error() {
    let token = "a.b.c";
    for bad in [
        rmpv::Value::from(1),
        rmpv::Value::from("not json"),
        rmpv::Value::from("[]"),
        map(vec![("keys", "x".into())]),
        jwks(vec![map(vec![]); 65]),
    ] {
        let e = verify(&verify_only_scope(), token, bad.clone()).unwrap_err();
        assert!(e.contains("args.jwks"), "{bad:?}: {e}");
    }
}

/// verify_only holds no key: it answers random, hash and jwt_verify with
/// jwks, refuses every keyed call by name, and stands alone in its scope.
#[test]
fn verify_only_answers_what_needs_no_key_and_refuses_the_rest() {
    let sc = verify_only_scope();
    assert!(call(&sc, "crypto/random", map(vec![("bytes", 8.into())])).is_ok());
    assert!(call(&sc, "crypto/hash", map(vec![("data", "x".into())])).is_ok());
    for name in [
        "crypto/jwt_sign",
        "crypto/hmac",
        "crypto/derive",
        "crypto/totp",
    ] {
        let e = call(&sc, name, map(vec![("claims", map(vec![]))])).unwrap_err();
        assert!(e.contains("verify_only"), "{name}: {e}");
    }
    let e = call(
        &sc,
        "crypto/jwt_verify",
        map(vec![("token", "a.b.c".into())]),
    )
    .unwrap_err();
    assert!(e.contains("args.jwks"), "{e}");

    let scope_type = CryptoConnector::new().scope_type();
    assert!(scope_type.validate(Some(&sc)).is_ok());
    for bad in [
        map(vec![("verify_only", true.into()), ("key", MASTER.into())]),
        map(vec![("verify_only", false.into())]),
    ] {
        let e = scope_type.validate(Some(&Scope(bad))).unwrap_err();
        assert!(e.contains("verify_only"), "{e}");
    }
}
