//! EdDSA (RFC 8037) under `jwt_sign`, `jwt_verify` and `public_key`: the
//! published key, a standard JWT library reading what this host writes,
//! the derived keypair pinned to the public KDF labels, and the forgeries
//! that the key-picks-the-algorithm rule exists to refuse.
//!
//! # Surface
//!
//! Entry points: the tests.
//!
//! Configurable values:
//! - `DEV_KEY`, the master.
//! - `RFC8037_D`, `RFC8037_X`: RFC 8037 Appendix A.1's private and public
//!   key, base64url, wired as the `rfc` signing key.
//! - `RFC8037_A4_INPUT`, `RFC8037_A4_SIG`: Appendix A.4's signing input
//!   and signature.
//!
//! Fan-out: none.

use base64::Engine as _;
use drt_caps::Scope;
use drt_connector::{Asker, Caller, Connector};
use drt_connector_crypto::{CryptoConnector, KDF_LABEL_DERIVE, KDF_LABEL_EDDSA};
use hmac::{Mac, SimpleHmac};
use sha2::Sha256;

const DEV_KEY: &str = "capability-testing-dev-key-0123456789";
const PARTNER_SECRET: &str = "the-partners-shared-webhook-secret";
const RFC8037_D: &str = "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A";
const RFC8037_X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
const RFC8037_A4_INPUT: &str = "eyJhbGciOiJFZERTQSJ9.RXhhbXBsZSBvZiBFZDI1NTE5IHNpZ25pbmc";
const RFC8037_A4_SIG: &str =
    "hgyY0il_MGCjP0JzlnLWG1PPOt7-09PGcvMg3AIbQR6dWbhijcNR4ki4iylGjg5BhVsPt9g7sVvpAr_MuM0KAg";

// depth: the scope, the calls, and the independent derivation

const B64URL: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A PKCS#8 PEM for a raw Ed25519 secret, built here by hand rather than
/// by the library the connector parses with: RFC 8410's fixed 16-byte
/// prefix, then the 32 bytes.
fn pkcs8_der(secret: &[u8]) -> Vec<u8> {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(secret);
    der
}

fn pkcs8_pem(secret: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(pkcs8_der(secret));
    format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n")
}

fn rfc_pem() -> String {
    pkcs8_pem(&B64URL.decode(RFC8037_D).unwrap())
}

fn map(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn signing_key(name: &str, private_key: rmpv::Value) -> rmpv::Value {
    map(vec![
        ("name", name.into()),
        ("alg", "EdDSA".into()),
        ("private_key", private_key),
    ])
}

fn scope_with(master: &str, signing_keys: Vec<rmpv::Value>, unkeyed: Option<bool>) -> Scope {
    let mut entries = vec![
        ("key", master.into()),
        (
            "secrets",
            rmpv::Value::Array(vec![map(vec![
                ("name", "partner-x".into()),
                ("key", PARTNER_SECRET.into()),
            ])]),
        ),
        ("signing_keys", rmpv::Value::Array(signing_keys)),
    ];
    if let Some(on) = unkeyed {
        entries.push((
            "jwt",
            map(vec![("accept_unkeyed", rmpv::Value::Boolean(on))]),
        ));
    }
    Scope(map(entries))
}

fn scope() -> Scope {
    scope_with(
        DEV_KEY,
        vec![signing_key("rfc", rfc_pem().as_str().into())],
        None,
    )
}

fn call_as(
    c: &CryptoConnector,
    caller: Caller,
    sc: &Scope,
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let grants = [Scope(rmpv::Value::from("issuer:*"))];
    let asker = Asker {
        caller,
        grants: &grants,
    };
    pollster::block_on(c.call_as(&asker, name, Some(a), Some(sc))).map_err(|e| e.to_string())
}

const NODE: Caller = Caller::Node(1);

fn derive(c: &CryptoConnector, sc: &Scope, label: &str, alg: Option<&str>) -> Result<(), String> {
    let mut a = vec![("label", label.into())];
    if let Some(alg) = alg {
        a.push(("alg", alg.into()));
    }
    call_as(c, NODE, sc, "crypto/derive", map(a)).map(|_| ())
}

fn sign(c: &CryptoConnector, sc: &Scope, key: &str) -> String {
    call_as(
        c,
        NODE,
        sc,
        "crypto/jwt_sign",
        map(vec![
            (
                "claims",
                map(vec![("sub", "alice".into()), ("n", 7.into())]),
            ),
            ("ttl", 600.into()),
            ("key", key.into()),
        ]),
    )
    .unwrap()
    .as_str()
    .unwrap()
    .to_string()
}

fn verify_with(c: &CryptoConnector, caller: Caller, sc: &Scope, token: &str) -> rmpv::Value {
    call_as(
        c,
        caller,
        sc,
        "crypto/jwt_verify",
        map(vec![("token", token.into())]),
    )
    .expect("a verdict is an answer")
}

fn verify(c: &CryptoConnector, sc: &Scope, token: &str) -> rmpv::Value {
    verify_with(c, NODE, sc, token)
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

fn public_key(c: &CryptoConnector, sc: &Scope, key: &str) -> Result<rmpv::Value, String> {
    call_as(
        c,
        NODE,
        sc,
        "crypto/public_key",
        map(vec![("key", key.into())]),
    )
}

/// The JWK as JSON text, the way a program's JWKS endpoint would serve it.
fn jwk_json(jwk: &rmpv::Value) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (k, v) in jwk.as_map().unwrap() {
        obj.insert(
            k.as_str().unwrap().to_string(),
            serde_json::Value::String(v.as_str().unwrap().to_string()),
        );
    }
    serde_json::Value::Object(obj)
}

/// A token assembled by hand: `header` JSON, `payload` JSON, and the
/// signature segment `sign` returns over the signing input.
fn forge(header: &str, payload: &str, sign: impl Fn(&[u8]) -> Vec<u8>) -> String {
    let input = format!(
        "{}.{}",
        B64URL.encode(header.as_bytes()),
        B64URL.encode(payload.as_bytes())
    );
    let sig = sign(input.as_bytes());
    format!("{input}.{}", B64URL.encode(sig))
}

fn live_payload() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!(r#"{{"sub":"mallory","iat":{now},"exp":{}}}"#, now + 600)
}

fn hmac256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = <SimpleHmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// RFC 8037, and a standard library reading what this host writes
// ---------------------------------------------------------------------------

/// Appendix A.1's key, configured as PKCS#8, answers Appendix A.2's public
/// key, and A.4's published signature verifies under the `x` this host
/// answered: the JWK is the RFC's key, not one that merely round-trips.
#[test]
fn the_rfc8037_key_answers_the_rfc8037_jwk() {
    let c = CryptoConnector::new();
    let jwk = public_key(&c, &scope(), "rfc").unwrap();
    assert_eq!(field(&jwk, "kty").as_str(), Some("OKP"));
    assert_eq!(field(&jwk, "crv").as_str(), Some("Ed25519"));
    assert_eq!(field(&jwk, "x").as_str(), Some(RFC8037_X));
    assert_eq!(field(&jwk, "kid").as_str(), Some("rfc"));
    assert_eq!(field(&jwk, "alg").as_str(), Some("EdDSA"));
    assert_eq!(field(&jwk, "use").as_str(), Some("sig"));
    // No private member, under any spelling a JWK has for one.
    assert_eq!(field(&jwk, "d"), &rmpv::Value::Nil);
    assert_eq!(jwk.as_map().unwrap().len(), 6);

    let x: [u8; 32] = B64URL
        .decode(field(&jwk, "x").as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&x).unwrap();
    let sig: [u8; 64] = B64URL.decode(RFC8037_A4_SIG).unwrap().try_into().unwrap();
    vk.verify_strict(
        RFC8037_A4_INPUT.as_bytes(),
        &ed25519_dalek::Signature::from_bytes(&sig),
    )
    .expect("RFC 8037 A.4 verifies under the answered x");
}

/// What the RFC key signs carries the fixed EdDSA header, verifies here,
/// and verifies in `jsonwebtoken` from the JWK alone, which is exactly
/// what a service reading this host's JWKS does.
#[test]
fn a_configured_eddsa_token_verifies_in_a_standard_library_from_the_jwk() {
    let c = CryptoConnector::new();
    let sc = scope();
    let token = sign(&c, &sc, "rfc");
    let header = token.split('.').next().unwrap();
    assert_eq!(
        B64URL.decode(header).unwrap(),
        br#"{"alg":"EdDSA","typ":"JWT","kid":"rfc"}"#
    );
    let v = verify(&c, &sc, &token);
    assert!(valid(&v), "{v:?}");
    assert_eq!(field(field(&v, "claims"), "sub").as_str(), Some("alice"));

    let jwk: jsonwebtoken::jwk::Jwk =
        serde_json::from_value(jwk_json(&public_key(&c, &sc, "rfc").unwrap())).unwrap();
    let key = jsonwebtoken::DecodingKey::from_jwk(&jwk).unwrap();
    let decoded = jsonwebtoken::decode::<serde_json::Value>(
        &token,
        &key,
        &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA),
    )
    .expect("jsonwebtoken accepts the token under the JWK");
    assert_eq!(decoded.header.kid.as_deref(), Some("rfc"));
    assert_eq!(decoded.header.alg, jsonwebtoken::Algorithm::EdDSA);
    assert_eq!(decoded.claims["sub"], "alice");
    assert_eq!(decoded.claims["n"], 7);
    let exp = decoded.claims["exp"].as_i64().unwrap();
    let iat = decoded.claims["iat"].as_i64().unwrap();
    assert_eq!(exp - iat, 600);
}

/// And the other way: a token `jsonwebtoken` signs with the RFC key, in
/// this host's header, verifies here. Ed25519 is deterministic, so the
/// two signers must agree byte for byte on the same input.
#[test]
fn a_standard_library_and_this_host_agree_on_the_signature() {
    let c = CryptoConnector::new();
    let sc = scope();
    let ours = sign(&c, &sc, "rfc");
    let (input, sig) = ours.rsplit_once('.').unwrap();
    let der = pkcs8_der(&B64URL.decode(RFC8037_D).unwrap());
    let key = jsonwebtoken::EncodingKey::from_ed_der(&der);
    let theirs =
        jsonwebtoken::crypto::sign(input.as_bytes(), &key, jsonwebtoken::Algorithm::EdDSA).unwrap();
    assert_eq!(sig, theirs);
}

// ---------------------------------------------------------------------------
// A derived EdDSA label
// ---------------------------------------------------------------------------

/// The label's keypair is `HMAC(label master, KDF_LABEL_EDDSA)`, computed
/// here from the public labels alone: the JWKS a deployment publishes is a
/// function of its master and the label's name, and nothing else.
#[test]
fn a_derived_eddsa_key_is_pinned_to_the_public_kdf() {
    let c = CryptoConnector::new();
    let sc = scope();
    derive(&c, &sc, "issuer:1", Some("EdDSA")).unwrap();
    let jwk = public_key(&c, &sc, "issuer:1").unwrap();

    let k_label = hmac256(DEV_KEY.as_bytes(), KDF_LABEL_DERIVE);
    let label_master = hmac256(&k_label, b"issuer:1");
    let secret: [u8; 32] = hmac256(&label_master, KDF_LABEL_EDDSA).try_into().unwrap();
    let want = ed25519_dalek::SigningKey::from_bytes(&secret).verifying_key();
    assert_eq!(
        field(&jwk, "x").as_str().unwrap(),
        B64URL.encode(want.to_bytes())
    );

    let token = sign(&c, &sc, "issuer:1");
    assert!(valid(&verify(&c, &sc, &token)));
    let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk_json(&jwk)).unwrap();
    jsonwebtoken::decode::<serde_json::Value>(
        &token,
        &jsonwebtoken::DecodingKey::from_jwk(&jwk).unwrap(),
        &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA),
    )
    .expect("a derived label's token verifies from its JWK");
}

/// Two hosts on one master reach one keypair for a label, so the signer
/// and the verifier need not be the same process; another master reaches
/// another.
#[test]
fn a_derived_eddsa_key_is_the_same_on_every_host_with_the_master() {
    let a = CryptoConnector::new();
    let b = CryptoConnector::new();
    let sc = scope();
    derive(&a, &sc, "issuer:1", Some("EdDSA")).unwrap();
    derive(&b, &sc, "issuer:1", Some("EdDSA")).unwrap();
    let token = sign(&a, &sc, "issuer:1");
    assert!(valid(&verify(&b, &sc, &token)));
    assert_eq!(
        public_key(&a, &sc, "issuer:1").unwrap(),
        public_key(&b, &sc, "issuer:1").unwrap()
    );

    let other = scope_with("another-master-key-0123456789abc", vec![], None);
    let o = CryptoConnector::new();
    derive(&o, &other, "issuer:1", Some("EdDSA")).unwrap();
    assert_ne!(
        field(&public_key(&o, &other, "issuer:1").unwrap(), "x"),
        field(&public_key(&a, &sc, "issuer:1").unwrap(), "x")
    );
    assert_eq!(reason(&verify(&o, &other, &token)), "signature");
}

/// An EdDSA label is per caller like any label; a configured signing key
/// is the deployment's and reachable by every caller.
#[test]
fn a_label_is_the_callers_and_a_signing_key_is_the_deployments() {
    let c = CryptoConnector::new();
    let sc = scope();
    derive(&c, &sc, "issuer:1", Some("EdDSA")).unwrap();
    let labelled = sign(&c, &sc, "issuer:1");
    assert_eq!(
        reason(&verify_with(&c, Caller::Node(2), &sc, &labelled)),
        "kid"
    );
    let configured = sign(&c, &sc, "rfc");
    assert!(valid(&verify_with(&c, Caller::Node(2), &sc, &configured)));
    assert!(valid(&verify_with(&c, Caller::Root, &sc, &configured)));
}

/// `accept_unkeyed` bounds the constant HS256 header and nothing else:
/// with it off, an EdDSA token, which always names its key, still
/// verifies.
#[test]
fn accept_unkeyed_off_leaves_eddsa_alone() {
    let c = CryptoConnector::new();
    let sc = scope_with(
        DEV_KEY,
        vec![signing_key("rfc", rfc_pem().as_str().into())],
        Some(false),
    );
    let token = sign(&c, &sc, "rfc");
    assert!(valid(&verify(&c, &sc, &token)));
}

// ---------------------------------------------------------------------------
// The forgeries: the key picks the algorithm
// ---------------------------------------------------------------------------

/// RS256/HS256 confusion in its EdDSA form: a token that claims HS256 and
/// is MACed under the EdDSA key's public bytes, raw or as its `x`. The
/// key `rfc` is EdDSA, so the header is rebuilt as EdDSA and the forged
/// one does not match; the MAC is never computed.
#[test]
fn hs256_under_the_public_key_is_refused_as_alg() {
    let c = CryptoConnector::new();
    let sc = scope();
    let x = B64URL.decode(RFC8037_X).unwrap();
    for key in [x.clone(), RFC8037_X.as_bytes().to_vec()] {
        let token = forge(
            r#"{"alg":"HS256","typ":"JWT","kid":"rfc"}"#,
            &live_payload(),
            |input| hmac256(&key, input),
        );
        assert_eq!(reason(&verify(&c, &sc, &token)), "alg");
    }
}

#[test]
fn alg_none_naming_an_eddsa_key_is_refused_as_alg() {
    let c = CryptoConnector::new();
    let sc = scope();
    for header in [
        r#"{"alg":"none","typ":"JWT","kid":"rfc"}"#,
        r#"{"alg":"none","kid":"rfc"}"#,
    ] {
        let token = forge(header, &live_payload(), |_| Vec::new());
        assert_eq!(reason(&verify(&c, &sc, &token)), "alg");
    }
}

/// The other direction: an EdDSA header naming an HS256 label, signed by
/// some Ed25519 key the forger holds. The label is HS256, so the EdDSA
/// header is not the one it writes.
#[test]
fn eddsa_naming_an_hs256_label_is_refused_as_alg() {
    let c = CryptoConnector::new();
    let sc = scope();
    derive(&c, &sc, "issuer:hs", None).unwrap();
    let mallory = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let token = forge(
        r#"{"alg":"EdDSA","typ":"JWT","kid":"issuer:hs"}"#,
        &live_payload(),
        |input| {
            use ed25519_dalek::Signer as _;
            mallory.sign(input).to_bytes().to_vec()
        },
    );
    assert_eq!(reason(&verify(&c, &sc, &token)), "alg");
}

/// The right header and another key's signature; and this host's own
/// token with its payload swapped.
#[test]
fn another_keys_signature_and_a_swapped_payload_are_refused_as_signature() {
    let c = CryptoConnector::new();
    let sc = scope();
    let mallory = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let token = forge(
        r#"{"alg":"EdDSA","typ":"JWT","kid":"rfc"}"#,
        &live_payload(),
        |input| {
            use ed25519_dalek::Signer as _;
            mallory.sign(input).to_bytes().to_vec()
        },
    );
    assert_eq!(reason(&verify(&c, &sc, &token)), "signature");

    let ours = sign(&c, &sc, "rfc");
    let parts: Vec<&str> = ours.split('.').collect();
    let swapped = format!(
        "{}.{}.{}",
        parts[0],
        B64URL.encode(live_payload().as_bytes()),
        parts[2]
    );
    assert_eq!(reason(&verify(&c, &sc, &swapped)), "signature");
}

/// One token, one spelling: the signature segment's last character carries
/// two unused bits, and setting them, truncating, or padding the segment
/// is refused rather than read as the same signature.
#[test]
fn a_signature_with_another_spelling_is_refused() {
    let c = CryptoConnector::new();
    let sc = scope();
    let ours = sign(&c, &sc, "rfc");
    let (input, sig) = ours.rsplit_once('.').unwrap();
    let last = sig.chars().last().unwrap();
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let i = alphabet.find(last).unwrap();
    let respelled = alphabet.chars().nth(i ^ 1).unwrap();
    for bad in [
        format!("{}{respelled}", &sig[..sig.len() - 1]),
        sig[..sig.len() - 1].to_string(),
        format!("{sig}=="),
        format!("{sig}A"),
    ] {
        assert_eq!(
            reason(&verify(&c, &sc, &format!("{input}.{bad}"))),
            "signature"
        );
    }
}

// ---------------------------------------------------------------------------
// Refusals: the call, the label, the config
// ---------------------------------------------------------------------------

#[test]
fn public_key_refuses_an_hs256_key_and_an_unknown_name() {
    let c = CryptoConnector::new();
    let sc = scope();
    derive(&c, &sc, "issuer:hs", None).unwrap();
    let err = public_key(&c, &sc, "issuer:hs").unwrap_err();
    assert!(err.contains("HS256 key, which has no public half"), "{err}");
    let err = public_key(&c, &sc, "issuer:none").unwrap_err();
    assert!(
        err.contains("derived no label named 'issuer:none'"),
        "{err}"
    );
    // A configured secret is neither, and says so the same way.
    let err = public_key(&c, &sc, "partner-x").unwrap_err();
    assert!(err.contains("no signing key by that name"), "{err}");
}

#[test]
fn derive_takes_one_alg_per_label_and_no_configured_name() {
    let c = CryptoConnector::new();
    let sc = scope();
    derive(&c, &sc, "issuer:1", Some("EdDSA")).unwrap();
    // Idempotent with the same alg.
    derive(&c, &sc, "issuer:1", Some("EdDSA")).unwrap();
    let err = derive(&c, &sc, "issuer:1", None).unwrap_err();
    assert!(
        err.contains("already derived here with alg 'EdDSA', not 'HS256'"),
        "{err}"
    );
    let err = derive(&c, &sc, "issuer:2", Some("RS256")).unwrap_err();
    assert!(err.contains("'HS256' (the default) or 'EdDSA'"), "{err}");

    // A label may not shadow a configured signing key. The grant here
    // names it, so it is the name that refuses and not the scope.
    let grants = [Scope(rmpv::Value::from("rfc"))];
    let asker = Asker {
        caller: NODE,
        grants: &grants,
    };
    let err = pollster::block_on(c.call_as(
        &asker,
        "crypto/derive",
        Some(map(vec![("label", "rfc".into())])),
        Some(&sc),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("already a configured signing key"), "{err}");
}

fn wiring_error(sc: &Scope) -> String {
    let c = CryptoConnector::new();
    pollster::block_on(c.call("crypto/random", Some(map(vec![])), Some(sc)))
        .unwrap_err()
        .to_string()
}

#[test]
fn a_signing_key_is_refused_at_wiring_by_name() {
    let pem = rfc_pem();
    let bad_alg = map(vec![
        ("name", "k".into()),
        ("alg", "RS256".into()),
        ("private_key", pem.as_str().into()),
    ]);
    let err = wiring_error(&scope_with(DEV_KEY, vec![bad_alg], None));
    assert!(
        err.contains("crypto.signing_keys['k']: alg 'RS256'"),
        "{err}"
    );

    let err = wiring_error(&scope_with(
        DEV_KEY,
        vec![signing_key("k", "not a pem".into())],
        None,
    ));
    assert!(
        err.contains("crypto.signing_keys['k'].private_key: not an Ed25519 PKCS#8"),
        "{err}"
    );

    let err = wiring_error(&scope_with(
        DEV_KEY,
        vec![signing_key("partner-x", pem.as_str().into())],
        None,
    ));
    assert!(err.contains("already used"), "{err}");

    let err = wiring_error(&scope_with(
        DEV_KEY,
        vec![
            signing_key("k", pem.as_str().into()),
            signing_key("k", pem.as_str().into()),
        ],
        None,
    ));
    assert!(err.contains("already used"), "{err}");
}

/// The secret-value shape the root config uses (`drt_config::Secret`):
/// a string, `{env}`, `{file}` less one newline; `{prompt}` refused by
/// name, since a scope has no terminal; anything else refused naming the
/// field.
#[test]
fn a_signing_key_takes_the_secret_value_shape() {
    let pem = rfc_pem();
    let dir = std::env::temp_dir().join(format!("drt-eddsa-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("key.pem");
    // `pem` already ends in a newline; a second is what `echo >>` leaves.
    std::fs::write(&file, format!("{pem}\n")).unwrap();
    let var = format!("DRT_TEST_EDDSA_KEY_{}", std::process::id());
    std::env::set_var(&var, &pem);

    for source in [
        map(vec![("file", file.to_str().unwrap().into())]),
        map(vec![("env", var.as_str().into())]),
    ] {
        let sc = scope_with(DEV_KEY, vec![signing_key("rfc", source)], None);
        let c = CryptoConnector::new();
        let jwk = public_key(&c, &sc, "rfc").unwrap();
        assert_eq!(field(&jwk, "x").as_str(), Some(RFC8037_X));
    }

    for (source, expect) in [
        (map(vec![("prompt", "hidden".into())]), "cannot prompt"),
        (
            map(vec![("env", "DRT_TEST_UNSET_VARIABLE_XYZ".into())]),
            "is not set",
        ),
        (
            map(vec![("file", "/nonexistent/key.pem".into())]),
            "cannot read",
        ),
        (map(vec![("envv", "X".into())]), "unknown source 'envv'"),
        (
            map(vec![("env", "A".into()), ("file", "B".into())]),
            "exactly one of",
        ),
        (rmpv::Value::from(5), "a secret value is a string"),
    ] {
        let err = wiring_error(&scope_with(DEV_KEY, vec![signing_key("rfc", source)], None));
        assert!(
            err.contains("crypto.signing_keys['rfc'].private_key"),
            "{err}"
        );
        assert!(err.contains(expect), "{expect}: {err}");
    }
    std::env::remove_var(&var);
    std::fs::remove_dir_all(&dir).ok();
}
