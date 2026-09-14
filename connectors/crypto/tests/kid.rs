//! `kid` in the JWT header, and the key selected from the label it names
//! (`doc/Plan-0.7.0.md` §5), at the connector boundary.
//!
//! # Surface
//!
//! Entry points: the tests. Each calls the connector as a node would, with
//! an `Asker`, the way `derive.rs` does.
//!
//! Configurable values:
//! - `DEV_KEY` — the master the connector is wired with.
//! - `PARTNER_SECRET` — the one `crypto.secrets` entry, a peer's bytes.
//!
//! Fan-out: none.
//!
//! The hand-assembled tokens below are signed from keys this file derives
//! **independently**, from the public KDF labels: a test that asked the
//! connector which key it used would be asking the thing under test to
//! grade itself. Acceptance 16 is the one that fails without the subkey
//! split; acceptance 21 is the `kid` cases; acceptance 17 is the refusal
//! of a shared secret by name.

use std::sync::Arc;

use base64::Engine as _;
use drt_caps::Scope;
use drt_connector::{Asker, Caller, Connector, Registry};
use drt_connector_crypto::{
    CryptoConnector, JWT_HEADER_B64, KDF_LABEL_DERIVE, KDF_LABEL_HMAC, KDF_LABEL_JWT,
};
use hmac::{Mac, SimpleHmac};
use sha2::Sha256;

const DEV_KEY: &str = "capability-testing-dev-key-0123456789";
const PARTNER_SECRET: &str = "the-partners-shared-webhook-secret";

// depth: the scope, the calls, and the independent derivation

/// The wiring: the master, one configured secret, and — when asked — a
/// `jwt` block with the switch set.
fn scope_with(accept_unkeyed: Option<bool>) -> Scope {
    let mut entries = vec![
        ("key".into(), DEV_KEY.into()),
        (
            "secrets".into(),
            rmpv::Value::Array(vec![rmpv::Value::Map(vec![
                ("name".into(), "partner-x".into()),
                ("key".into(), PARTNER_SECRET.into()),
            ])]),
        ),
    ];
    if let Some(on) = accept_unkeyed {
        entries.push((
            "jwt".into(),
            rmpv::Value::Map(vec![("accept_unkeyed".into(), rmpv::Value::Boolean(on))]),
        ));
    }
    Scope(rmpv::Value::Map(entries))
}

fn scope() -> Scope {
    scope_with(None)
}

fn args(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn call_as(
    c: &CryptoConnector,
    caller: Caller,
    sc: &Scope,
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    // A derive grant wide enough for every label these tests name; the
    // other verbs read no scope off the grant.
    let grants = [Scope(rmpv::Value::from("room:*"))];
    let asker = Asker {
        caller,
        grants: &grants,
    };
    pollster::block_on(c.call_as(&asker, name, Some(a), Some(sc))).map_err(|e| e.to_string())
}

fn derive(c: &CryptoConnector, caller: Caller, label: &str) {
    call_as(
        c,
        caller,
        &scope(),
        "crypto/derive",
        args(vec![("label", label.into())]),
    )
    .expect("derive");
}

fn sign(c: &CryptoConnector, caller: Caller, key: Option<&str>) -> Result<String, String> {
    let mut a = vec![
        ("claims", args(vec![("sub", "alice".into())])),
        ("ttl", 600u64.into()),
    ];
    if let Some(key) = key {
        a.push(("key", key.into()));
    }
    call_as(c, caller, &scope(), "crypto/jwt_sign", args(a))
        .map(|v| v.as_str().unwrap().to_string())
}

fn verify_with(c: &CryptoConnector, caller: Caller, sc: &Scope, token: &str) -> rmpv::Value {
    call_as(
        c,
        caller,
        sc,
        "crypto/jwt_verify",
        args(vec![("token", token.into())]),
    )
    .expect("a verdict is an answer, not an error")
}

fn verify(c: &CryptoConnector, caller: Caller, token: &str) -> rmpv::Value {
    verify_with(c, caller, &scope(), token)
}

fn field<'a>(v: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&rmpv::Value::Nil)
}

fn valid(v: &rmpv::Value) -> bool {
    field(v, "valid") == &rmpv::Value::Boolean(true)
}

fn reason(v: &rmpv::Value) -> &str {
    field(v, "reason").as_str().unwrap_or("")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .expect("base64url")
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = <SimpleHmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// A label's two subkeys, from the public labels and nothing else — the
/// derivation `ownership.rs` also pins against the snapshot.
fn label_keys(label: &str) -> ([u8; 32], [u8; 32]) {
    let k_label = hmac_sha256(DEV_KEY.as_bytes(), KDF_LABEL_DERIVE);
    let master = hmac_sha256(&k_label, label.as_bytes());
    (
        hmac_sha256(&master, KDF_LABEL_HMAC),
        hmac_sha256(&master, KDF_LABEL_JWT),
    )
}

/// The header a token signed under `label` carries, as this file expects
/// the connector to build it.
fn kid_header(label: &str) -> String {
    b64(format!(r#"{{"alg":"HS256","typ":"JWT","kid":"{label}"}}"#).as_bytes())
}

/// A payload the connector would accept: `exp` ten minutes out.
fn payload() -> String {
    let far = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    b64(format!(r#"{{"sub":"forged","exp":{far}}}"#).as_bytes())
}

/// `header.payload` signed under `key`, by hand.
fn assemble(header: &str, payload: &str, key: &[u8]) -> String {
    let input = format!("{header}.{payload}");
    format!("{input}.{}", b64(&hmac_sha256(key, input.as_bytes())))
}

fn split(token: &str) -> (&str, &str, &str) {
    let mut parts = token.split('.');
    (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    )
}

// ---------------------------------------------------------------------------
// The shape: kid in the header, the label's JWT subkey under it
// ---------------------------------------------------------------------------

/// §5.2: a token signed under a label carries the label as `kid`, in a
/// header whose members are always in the same order, and is signed with
/// the label's **JWT** subkey — which this test derives itself.
#[test]
fn a_keyed_token_carries_kid_and_is_signed_under_the_labels_jwt_subkey() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    derive(&c, node, "room:1");
    let token = sign(&c, node, Some("room:1")).unwrap();
    let (header, payload, sig) = split(&token);

    assert_eq!(
        String::from_utf8(unb64(header)).unwrap(),
        r#"{"alg":"HS256","typ":"JWT","kid":"room:1"}"#
    );
    assert_ne!(
        header, JWT_HEADER_B64,
        "the constant header is the unkeyed one"
    );
    // A second token has byte-identical header: the member order is fixed.
    let again = sign(&c, node, Some("room:1")).unwrap();
    assert_eq!(split(&again).0, header);

    let (_, l_jwt) = label_keys("room:1");
    let input = format!("{header}.{payload}");
    assert_eq!(
        sig,
        b64(&hmac_sha256(&l_jwt, input.as_bytes())),
        "the signature is not the label's JWT subkey over header.payload"
    );

    let v = verify(&c, node, &token);
    assert!(valid(&v), "{v}");
    assert_eq!(field(field(&v, "claims"), "sub").as_str(), Some("alice"));
}

/// Without `key`, nothing changed: the constant header, the default key,
/// and the same bytes the C host would emit for the same claims.
#[test]
fn an_unkeyed_token_is_todays_token() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    let token = sign(&c, node, None).unwrap();
    let (header, payload, sig) = split(&token);
    assert_eq!(header, JWT_HEADER_B64);
    let k_jwt = hmac_sha256(DEV_KEY.as_bytes(), KDF_LABEL_JWT);
    let input = format!("{header}.{payload}");
    assert_eq!(sig, b64(&hmac_sha256(&k_jwt, input.as_bytes())));
    assert!(valid(&verify(&c, node, &token)));
}

// ---------------------------------------------------------------------------
// Acceptance 17: a shared secret cannot back a token
// ---------------------------------------------------------------------------

/// `jwt_sign` naming a `crypto.secrets` entry is refused by name, saying
/// those bytes are shared with a peer — before the labels are consulted.
#[test]
fn a_configured_secret_is_refused_as_a_signing_key_by_name() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    let err = sign(&c, node, Some("partner-x")).unwrap_err();
    assert!(err.contains("'partner-x'"), "{err}");
    assert!(err.contains("shared with a peer"), "{err}");
    assert!(err.contains("crypto.secrets"), "{err}");

    // The secret still MACs, raw, for the interop it exists for.
    call_as(
        &c,
        node,
        &scope(),
        "crypto/hmac",
        args(vec![("data", "body".into()), ("key", "partner-x".into())]),
    )
    .expect("hmac under a configured secret is unchanged");

    // A label this caller did not derive reads as hmac's refusal does.
    let err = sign(&c, node, Some("room:1")).unwrap_err();
    assert!(err.contains("derived no label"), "{err}");
    assert!(err.contains("'room:1'"), "{err}");

    // And a key that is not a name is a caller error.
    let err = call_as(
        &c,
        node,
        &scope(),
        "crypto/jwt_sign",
        args(vec![
            ("claims", args(vec![("sub", "x".into())])),
            ("key", 7u64.into()),
        ]),
    )
    .unwrap_err();
    assert!(err.contains("args.key"), "{err}");
}

// ---------------------------------------------------------------------------
// Acceptance 16: hmac under a label is not a JWT oracle for it
// ---------------------------------------------------------------------------

/// The negative that makes the positive mean anything: a token assembled
/// by hand — the same header, the same claims — and MAC'd through
/// `crypto/hmac {key = label}` under the same label does **not** verify,
/// because `hmac` signs with the label's hmac subkey and the JWT with its
/// JWT subkey (§4.2a). This is the test that fails without the split.
#[test]
fn a_token_maced_through_hmac_under_the_same_label_does_not_verify() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    derive(&c, node, "room:1");
    let header = kid_header("room:1");
    let payload = payload();
    let input = format!("{header}.{payload}");

    // What a guest holding host:crypto/hmac and the label can compute over
    // exactly the bytes a JWT signature covers.
    let oracle = call_as(
        &c,
        node,
        &scope(),
        "crypto/hmac",
        args(vec![
            ("data", input.as_str().into()),
            ("key", "room:1".into()),
        ]),
    )
    .unwrap();
    let oracle: Vec<u8> = oracle
        .as_str()
        .unwrap()
        .as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    let (l_hmac, l_jwt) = label_keys("room:1");
    assert_eq!(
        oracle,
        hmac_sha256(&l_hmac, input.as_bytes()),
        "hmac did not sign with the label's hmac subkey"
    );

    let forged = format!("{input}.{}", b64(&oracle));
    let v = verify(&c, node, &forged);
    assert!(!valid(&v), "the hmac subkey produced a JWT signature: {v}");
    assert_eq!(reason(&v), "signature");

    // The control: the same assembly under the JWT subkey verifies, so the
    // refusal above is the key and not the assembly.
    let v = verify(&c, node, &assemble(&header, &payload, &l_jwt));
    assert!(valid(&v), "{v}");
}

// ---------------------------------------------------------------------------
// Acceptance 21: the kid cases
// ---------------------------------------------------------------------------

/// A token's header swapped for another label's `kid` refuses: the header
/// matches that label's rebuild, so the MAC is checked under that label's
/// key, and it is not the key that signed.
#[test]
fn a_header_swapped_for_another_labels_kid_refuses() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    derive(&c, node, "room:1");
    derive(&c, node, "room:2");
    let token = sign(&c, node, Some("room:1")).unwrap();
    let (_, payload, sig) = split(&token);
    let swapped = format!("{}.{payload}.{sig}", kid_header("room:2"));
    let v = verify(&c, node, &swapped);
    assert!(!valid(&v), "{v}");
    assert_eq!(reason(&v), "signature");
}

/// An unknown `kid` refuses — and "unknown" is per caller, since a label
/// is per owner: the owner that derived it verifies, a sibling that did
/// not is refused, and a sibling that derives it too reaches the same key
/// (the cross-owner case §4.3 forces).
#[test]
fn an_unknown_kid_refuses_and_known_is_per_caller() {
    let c = CryptoConnector::new();
    let signer = Caller::Node(1);
    let stranger = Caller::Node(2);
    let verifier = Caller::Node(3);
    derive(&c, signer, "room:1");
    let token = sign(&c, signer, Some("room:1")).unwrap();

    let v = verify(&c, stranger, &token);
    assert!(!valid(&v), "{v}");
    assert_eq!(reason(&v), "kid");

    derive(&c, verifier, "room:1");
    assert!(valid(&verify(&c, verifier, &token)));

    // A kid nobody derived, over a correctly-signed body, is still unknown.
    let (_, l_jwt) = label_keys("room:9");
    let v = verify(
        &c,
        signer,
        &assemble(&kid_header("room:9"), &payload(), &l_jwt),
    );
    assert!(!valid(&v), "{v}");
    assert_eq!(reason(&v), "kid");
}

/// Two `kid` members, or the members in another order, fail the byte
/// compare against the rebuilt header — signed correctly under the right
/// key, so what refuses is the header and nothing after it.
#[test]
fn a_header_with_two_kids_or_reordered_members_refuses() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    derive(&c, node, "room:1");
    let (_, l_jwt) = label_keys("room:1");
    for header in [
        br#"{"alg":"HS256","typ":"JWT","kid":"room:1","kid":"room:1"}"#.as_slice(),
        br#"{"kid":"room:1","alg":"HS256","typ":"JWT"}"#.as_slice(),
        br#"{"alg":"none","typ":"JWT","kid":"room:1"}"#.as_slice(),
        br#"{"alg":"HS256","typ":"JWT","kid":"room:1","x":1}"#.as_slice(),
    ] {
        let token = assemble(&b64(header), &payload(), &l_jwt);
        let v = verify(&c, node, &token);
        assert!(
            !valid(&v),
            "{:?}: {v}",
            std::str::from_utf8(header).unwrap()
        );
        assert_eq!(
            reason(&v),
            "alg",
            "{:?}",
            std::str::from_utf8(header).unwrap()
        );
    }
    // The control: the members in the one order the connector emits.
    let v = verify(
        &c,
        node,
        &assemble(&kid_header("room:1"), &payload(), &l_jwt),
    );
    assert!(valid(&v), "{v}");
}

/// No `kid` verifies with `jwt.accept_unkeyed` on — the default — and
/// refuses with it off; a keyed token is untouched by the switch.
#[test]
fn a_token_with_no_kid_is_behind_accept_unkeyed() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    derive(&c, node, "room:1");
    let unkeyed = sign(&c, node, None).unwrap();
    let keyed = sign(&c, node, Some("room:1")).unwrap();

    assert!(valid(&verify(&c, node, &unkeyed)), "the default is on");
    assert!(valid(&verify_with(
        &c,
        node,
        &scope_with(Some(true)),
        &unkeyed
    )));

    let off = scope_with(Some(false));
    let v = verify_with(&c, node, &off, &unkeyed);
    assert!(!valid(&v), "{v}");
    assert_eq!(reason(&v), "unkeyed");
    assert!(
        valid(&verify_with(&c, node, &off, &keyed)),
        "the switch bounds the unkeyed branch and nothing else"
    );

    // Off does not loosen the header rule: a non-constant header with no
    // kid is still refused at the header, not at the switch.
    let v = verify_with(
        &c,
        node,
        &off,
        &assemble(
            &b64(br#"{"alg":"none","typ":"JWT"}"#),
            &payload(),
            &hmac_sha256(DEV_KEY.as_bytes(), KDF_LABEL_JWT),
        ),
    );
    assert_eq!(reason(&v), "alg");
}

// ---------------------------------------------------------------------------
// Wiring: the jwt block is checked like the rest of the scope
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_key_under_jwt_is_refused_at_wiring() {
    for (jwt, expect) in [
        (
            rmpv::Value::Map(vec![
                ("accept_unkeyed".into(), rmpv::Value::Boolean(false)),
                ("bogus".into(), rmpv::Value::from(1u64)),
            ]),
            "bogus",
        ),
        (
            rmpv::Value::Map(vec![("accept_unkeyed".into(), "no".into())]),
            "expected a boolean",
        ),
    ] {
        let sc = Scope(rmpv::Value::Map(vec![
            ("key".into(), DEV_KEY.into()),
            ("jwt".into(), jwt),
        ]));
        let mut reg = Registry::new();
        let err = reg
            .wire("crypto", Arc::new(CryptoConnector::new()), Some(sc))
            .unwrap_err()
            .to_string();
        assert!(err.contains(expect), "{err}");
    }
    // And the well-formed block wires.
    let mut reg = Registry::new();
    reg.wire(
        "crypto",
        Arc::new(CryptoConnector::new()),
        Some(scope_with(Some(false))),
    )
    .expect("a jwt block with only accept_unkeyed wires");
}
