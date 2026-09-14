//! `crypto/derive`: a runtime name for a key that stays in the host
//! (`doc/Plan-0.7.0.md` §4), at the connector boundary.
//!
//! What a guest-and-snapshot test cannot show and this one can: who may
//! name which label, that two owners reach one key, that a dead owner's
//! label reads as unknown, and that a configured secret cannot be shadowed.

use drt_caps::Scope;
use drt_connector::{Asker, Caller, Connector};
use drt_connector_crypto::CryptoConnector;

const DEV_KEY: &str = "capability-testing-dev-key-0123456789";

fn scope() -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("key".into(), DEV_KEY.into()),
        (
            "secrets".into(),
            rmpv::Value::Array(vec![rmpv::Value::Map(vec![
                ("name".into(), "partner-x".into()),
                ("key".into(), "the-partners-shared-webhook-secret".into()),
            ])]),
        ),
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

/// The scopes on a caller's `host:crypto/derive` grants, as the
/// dispatcher would hand them over: each pattern its own grant.
fn grants(patterns: &[&str]) -> Vec<Scope> {
    patterns
        .iter()
        .map(|p| Scope(rmpv::Value::from(*p)))
        .collect()
}

fn call_as(
    c: &CryptoConnector,
    caller: Caller,
    grants: &[Scope],
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let asker = Asker { caller, grants };
    pollster::block_on(c.call_as(&asker, name, Some(a), Some(&scope()))).map_err(|e| e.to_string())
}

fn derive(
    c: &CryptoConnector,
    caller: Caller,
    patterns: &[&str],
    label: &str,
) -> Result<rmpv::Value, String> {
    call_as(
        c,
        caller,
        &grants(patterns),
        "crypto/derive",
        args(vec![("label", label.into())]),
    )
}

fn hmac(c: &CryptoConnector, caller: Caller, key: &str, data: &str) -> Result<String, String> {
    call_as(
        c,
        caller,
        &[],
        "crypto/hmac",
        args(vec![("data", data.into()), ("key", key.into())]),
    )
    .map(|v| v.as_str().unwrap().to_string())
}

/// §4.2: the call returns nothing, and the name is then usable as a key.
#[test]
fn derive_returns_nothing_and_the_label_then_signs() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    let back = derive(&c, node, &["room:*"], "room:1").unwrap();
    assert_eq!(back, rmpv::Value::Nil, "nothing comes back: {back}");

    let mac = hmac(&c, node, "room:1", "hello").unwrap();
    assert_eq!(mac.len(), 64, "a hex SHA-256 MAC");
    assert!(!mac.contains(DEV_KEY));
}

/// Acceptance 18: two different owners deriving the same label reach the
/// same key, and a second derive of a live label is idempotent.
#[test]
fn two_owners_deriving_one_label_reach_one_key() {
    let c = CryptoConnector::new();
    let signer = Caller::Node(1);
    let verifier = Caller::Node(2);
    derive(&c, signer, &["room:*"], "room:7").unwrap();
    derive(&c, verifier, &["room:7"], "room:7").unwrap();

    let a = hmac(&c, signer, "room:7", "the message").unwrap();
    let b = hmac(&c, verifier, "room:7", "the message").unwrap();
    assert_eq!(a, b, "the verifier reaches the signer's key by name");

    // Again, from the signer: not a refusal, and the key did not move.
    derive(&c, signer, &["room:*"], "room:7").expect("idempotent");
    assert_eq!(hmac(&c, signer, "room:7", "the message").unwrap(), a);
}

/// Different labels are different keys, or the label is decoration.
#[test]
fn different_labels_are_different_keys() {
    let c = CryptoConnector::new();
    let node = Caller::Node(1);
    derive(&c, node, &["room:*"], "room:1").unwrap();
    derive(&c, node, &["room:*"], "room:2").unwrap();
    assert_ne!(
        hmac(&c, node, "room:1", "x").unwrap(),
        hmac(&c, node, "room:2", "x").unwrap()
    );
    // And neither is the default subkey.
    let default = call_as(
        &c,
        node,
        &[],
        "crypto/hmac",
        args(vec![("data", "x".into())]),
    )
    .unwrap()
    .as_str()
    .unwrap()
    .to_string();
    assert_ne!(hmac(&c, node, "room:1", "x").unwrap(), default);
}

/// Acceptance 19: the scope on the grant is the control. A node allowed
/// `room:2` cannot derive `room:3`, and the refusal names the scope
/// rather than whether `room:3` exists for someone else.
#[test]
fn a_label_outside_the_grants_scope_is_refused_by_the_scope() {
    let c = CryptoConnector::new();
    let manager = Caller::Node(1);
    let room2 = Caller::Node(2);
    derive(&c, manager, &["room:*"], "room:3").unwrap();

    derive(&c, room2, &["room:2"], "room:2").expect("its own");
    let err = derive(&c, room2, &["room:2"], "room:3").unwrap_err();
    assert!(err.contains("outside"), "{err}");
    assert!(err.contains("allows: room:2"), "names the scope: {err}");
    assert!(
        !err.contains("exists") && !err.contains("instance 1"),
        "must not say whether room:3 is live elsewhere: {err}"
    );

    // Prefix and exact are the capability grammar's own shapes.
    derive(&c, Caller::Node(3), &["room:*"], "room:anything").unwrap();
    derive(&c, Caller::Node(4), &["room:1"], "room:10").unwrap_err();

    // A grant with no scope names nothing, and says so.
    let err = derive(&c, Caller::Node(5), &[], "room:1").unwrap_err();
    assert!(err.contains("names no labels"), "{err}");
}

/// Acceptance 20, the lifetime half: a label dies with its owner and reads
/// as unknown afterwards — the same sentence as never-derived — while a
/// sibling's label is untouched. Hibernation is not release; the pump test
/// pins that, and here the only release is the explicit one.
#[test]
fn a_label_dies_with_its_owner_and_a_siblings_survives() {
    let c = CryptoConnector::new();
    let a = Caller::Node(1);
    let b = Caller::Node(2);
    derive(&c, a, &["room:*"], "room:1").unwrap();
    derive(&c, b, &["room:*"], "room:1").unwrap();
    let before = hmac(&c, a, "room:1", "x").unwrap();

    let lost = c.release(&a);
    assert!(
        lost.is_empty(),
        "a derived key is re-derivable; nothing is lost"
    );

    let gone = hmac(&c, a, "room:1", "x").unwrap_err();
    let never = hmac(&c, a, "room:nope", "x").unwrap_err();
    assert!(gone.contains("derived no label"), "{gone}");
    assert_eq!(
        gone.replace("room:1", "L"),
        never.replace("room:nope", "L"),
        "released and never-derived read alike"
    );

    assert_eq!(
        hmac(&c, b, "room:1", "x").unwrap(),
        before,
        "b's is untouched"
    );

    // Re-deriving after release reaches the same bytes: nothing was lost.
    derive(&c, a, &["room:*"], "room:1").unwrap();
    assert_eq!(hmac(&c, a, "room:1", "x").unwrap(), before);
}

/// Acceptance 20, the collision half: a label may not shadow a configured
/// secret, and the refusal names both.
#[test]
fn a_label_may_not_shadow_a_configured_secret() {
    let c = CryptoConnector::new();
    // `partner-*`, not a bare `*`: the capability grammar's wildcard needs a
    // character before the star, so a bare `*` is the literal name `*`.
    let err = derive(&c, Caller::Node(1), &["partner-*"], "partner-x").unwrap_err();
    assert!(err.contains("partner-x"), "{err}");
    assert!(err.contains("configured secret"), "{err}");
    assert!(err.contains("crypto.secrets"), "{err}");

    // The configured secret still signs, raw, as before.
    hmac(&c, Caller::Node(1), "partner-x", "x").unwrap();
}

/// A label is not visible to a node that did not derive it, even with the
/// right scope: the scope permits naming, and naming is `derive`.
#[test]
fn a_label_is_per_owner_until_derived() {
    let c = CryptoConnector::new();
    derive(&c, Caller::Node(1), &["room:*"], "room:1").unwrap();
    let err = hmac(&c, Caller::Node(2), "room:1", "x").unwrap_err();
    assert!(err.contains("derived no label"), "{err}");
}

/// The caller-blind `call` path is the root with no grants: it can hash
/// and hmac as before, and cannot derive.
#[test]
fn the_plain_call_path_cannot_derive() {
    let c = CryptoConnector::new();
    let err = pollster::block_on(c.call(
        "crypto/derive",
        Some(args(vec![("label", "room:1".into())])),
        Some(&scope()),
    ))
    .unwrap_err();
    assert!(err.to_string().contains("names no labels"), "{err}");
    pollster::block_on(c.call(
        "crypto/hmac",
        Some(args(vec![("data", "x".into())])),
        Some(&scope()),
    ))
    .expect("the old surface is unchanged");
}

#[test]
fn a_label_that_is_not_a_name_is_refused() {
    let c = CryptoConnector::new();
    let n = Caller::Node(1);
    assert!(derive(&c, n, &["*"], "").is_err());
    assert!(derive(&c, n, &["*"], &"x".repeat(300)).is_err());
    assert!(derive(&c, n, &["*"], "has\0nul").is_err());
    assert!(call_as(
        &c,
        n,
        &grants(&["*"]),
        "crypto/derive",
        args(vec![("label", 7u64.into())])
    )
    .is_err());
}
