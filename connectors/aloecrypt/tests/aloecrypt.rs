//! `host:aloecrypt/*` at the connector boundary: every parameter set round
//! trips, the refusals are answers or errors as documented, the label
//! patterns on a grant decide who uses a private key, and a keyless wiring
//! answers only the public calls.
//!
//! The interop half checks the connector's output with aloecrypt_core's
//! own struct API, which shares no code with this crate's msgpack layer.

use aloecrypt_core::document::{
    armor, armored_size, dearmor, public_key_document_size, read_signer_65, verify_detached_65,
    write_recipient_768, RECIPIENT_LABEL, SIGNATURE_LABEL, SIGNER_LABEL,
};
use aloecrypt_core::kem_api::{MlKem768Encapsulator, MLKEM_768_ENCAP_KEY_SZ};
use drt_caps::Scope;
use drt_connector::{Asker, Caller, Connector};
use drt_connector_aloecrypt::{ops, AloecryptConnector, DEFAULT_HASH_DOMAIN};

const MASTER: &str = "aloecrypt-connector-test-master-key-000000";

fn scope() -> Scope {
    Scope(rmpv::Value::Map(vec![("key".into(), MASTER.into())]))
}

fn args(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn grants(patterns: &[&str]) -> Vec<Scope> {
    patterns
        .iter()
        .map(|p| Scope(rmpv::Value::from(*p)))
        .collect()
}

fn call_in(
    c: &AloecryptConnector,
    wiring: Option<&Scope>,
    patterns: &[&str],
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let g = grants(patterns);
    let asker = Asker {
        caller: Caller::Root,
        grants: &g,
    };
    pollster::block_on(c.call_as(&asker, name, Some(a), wiring)).map_err(|e| e.to_string())
}

fn call(
    c: &AloecryptConnector,
    patterns: &[&str],
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    call_in(c, Some(&scope()), patterns, name, a)
}

fn get<'a>(v: &'a rmpv::Value, k: &str) -> &'a rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(key, _)| key.as_str() == Some(k))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no '{k}' in {v}"))
}

fn public_key(c: &AloecryptConnector, label: &str, alg: &str) -> String {
    let v = call(
        c,
        &[],
        "aloecrypt/public_key",
        args(vec![("key", label.into()), ("alg", alg.into())]),
    )
    .unwrap();
    assert_eq!(get(&v, "alg").as_str(), Some(alg));
    assert_eq!(get(&v, "address").as_str().unwrap().len(), 64);
    get(&v, "public_key").as_str().unwrap().to_string()
}

#[test]
fn every_signing_set_signs_and_verifies() {
    let c = AloecryptConnector::new();
    for alg in ["ml-dsa-44", "ml-dsa-65", "ml-dsa-87"] {
        let pk = public_key(&c, "doc:1", alg);
        let sig = call(
            &c,
            &["doc:*"],
            "aloecrypt/sign",
            args(vec![
                ("key", "doc:1".into()),
                ("alg", alg.into()),
                ("data", "hello".into()),
            ]),
        )
        .unwrap();
        let sig = sig.as_str().unwrap().to_string();
        assert!(
            sig.starts_with("-----BEGIN ALOECRYPT SIGNATURE-----"),
            "{sig}"
        );

        let check = |data: &str, key: &str| {
            call(
                &c,
                &[],
                "aloecrypt/verify",
                args(vec![
                    ("public_key", key.into()),
                    ("data", data.into()),
                    ("signature", sig.as_str().into()),
                ]),
            )
            .unwrap()
        };
        assert_eq!(
            get(&check("hello", &pk), "valid").as_bool(),
            Some(true),
            "{alg}"
        );
        let altered = check("hellp", &pk);
        assert_eq!(get(&altered, "valid").as_bool(), Some(false));
        assert_eq!(get(&altered, "reason").as_str(), Some("mismatch"));
        // Another label's key of the same set is not the signer.
        let other = public_key(&c, "doc:2", alg);
        assert_eq!(
            get(&check("hello", &other), "reason").as_str(),
            Some("mismatch")
        );
    }
}

#[test]
fn every_kem_set_encrypts_and_decrypts() {
    let c = AloecryptConnector::new();
    let payload = rmpv::Value::Binary((0u8..=255).collect());
    for alg in ["ml-kem-512", "ml-kem-768", "ml-kem-1024"] {
        let pk = public_key(&c, "inbox:alice", alg);
        let doc = call(
            &c,
            &[],
            "aloecrypt/encrypt",
            args(vec![
                ("public_key", pk.as_str().into()),
                ("data", payload.clone()),
            ]),
        )
        .unwrap();
        let doc = doc.as_str().unwrap().to_string();
        assert!(doc.starts_with("-----BEGIN ALOECRYPT ENCRYPTED-----"));
        let open = |label: &str, patterns: &[&str]| {
            call(
                &c,
                patterns,
                "aloecrypt/decrypt",
                args(vec![
                    ("key", label.into()),
                    ("alg", alg.into()),
                    ("document", doc.as_str().into()),
                ]),
            )
        };
        assert_eq!(open("inbox:alice", &["inbox:*"]).unwrap(), payload, "{alg}");
        let wrong = open("inbox:bob", &["inbox:*"]).unwrap_err();
        assert!(wrong.contains("does not open under 'inbox:bob'"), "{wrong}");
    }
}

#[test]
fn encryption_is_fresh_each_time() {
    let c = AloecryptConnector::new();
    let pk = public_key(&c, "inbox:alice", "ml-kem-768");
    let enc = || {
        call(
            &c,
            &[],
            "aloecrypt/encrypt",
            args(vec![
                ("public_key", pk.as_str().into()),
                ("data", "x".into()),
            ]),
        )
        .unwrap()
    };
    assert_ne!(enc(), enc());
}

#[test]
fn the_defaults_are_ml_dsa_65_and_ml_kem_768() {
    let c = AloecryptConnector::new();
    let sig = call(
        &c,
        &["d"],
        "aloecrypt/sign",
        args(vec![("key", "d".into()), ("data", "m".into())]),
    )
    .unwrap();
    let pk = public_key(&c, "d", "ml-dsa-65");
    let v = call(
        &c,
        &[],
        "aloecrypt/verify",
        args(vec![
            ("public_key", pk.as_str().into()),
            ("data", "m".into()),
            ("signature", sig),
        ]),
    )
    .unwrap();
    assert_eq!(get(&v, "valid").as_bool(), Some(true));

    let pk = public_key(&c, "k", "ml-kem-768");
    let doc = call(
        &c,
        &[],
        "aloecrypt/encrypt",
        args(vec![
            ("public_key", pk.as_str().into()),
            ("data", "m".into()),
        ]),
    )
    .unwrap();
    let out = call(
        &c,
        &["k"],
        "aloecrypt/decrypt",
        args(vec![("key", "k".into()), ("document", doc)]),
    )
    .unwrap();
    assert_eq!(out, rmpv::Value::Binary(b"m".to_vec()));
}

#[test]
fn keys_are_a_function_of_master_and_label() {
    let a = public_key(&AloecryptConnector::new(), "doc:1", "ml-dsa-65");
    let b = public_key(&AloecryptConnector::new(), "doc:1", "ml-dsa-65");
    assert_eq!(a, b, "two hosts with one master derive one key");
    assert_ne!(
        a,
        public_key(&AloecryptConnector::new(), "doc:2", "ml-dsa-65")
    );

    let other = Scope(rmpv::Value::Map(vec![(
        "key".into(),
        "a-different-master-key-for-this-test-0000".into(),
    )]));
    let v = call_in(
        &AloecryptConnector::new(),
        Some(&other),
        &[],
        "aloecrypt/public_key",
        args(vec![("key", "doc:1".into()), ("alg", "ml-dsa-65".into())]),
    )
    .unwrap();
    assert_ne!(get(&v, "public_key").as_str(), Some(a.as_str()));
}

#[test]
fn private_halves_need_a_grant_naming_the_label() {
    let c = AloecryptConnector::new();
    let sign = |patterns: &[&str]| {
        call(
            &c,
            patterns,
            "aloecrypt/sign",
            args(vec![("key", "doc:1".into()), ("data", "m".into())]),
        )
    };
    let none = sign(&[]).unwrap_err();
    assert!(none.contains("names no labels"), "{none}");
    let elsewhere = sign(&["room:*"]).unwrap_err();
    assert!(elsewhere.contains("which allows: room:*"), "{elsewhere}");
    assert!(sign(&["doc:1"]).is_ok());
    assert!(sign(&["doc:*"]).is_ok());

    let decrypt = call(
        &c,
        &["room:*"],
        "aloecrypt/decrypt",
        args(vec![("key", "doc:1".into()), ("document", "x".into())]),
    )
    .unwrap_err();
    assert!(
        decrypt.contains("outside this instance's host:aloecrypt/decrypt grant"),
        "{decrypt}"
    );
}

#[test]
fn a_keyless_wiring_answers_only_the_public_calls() {
    let keyed = AloecryptConnector::new();
    let pk = public_key(&keyed, "doc:1", "ml-dsa-65");
    let sig = call(
        &keyed,
        &["doc:1"],
        "aloecrypt/sign",
        args(vec![("key", "doc:1".into()), ("data", "m".into())]),
    )
    .unwrap();

    let keyless = AloecryptConnector::new();
    let v = call_in(
        &keyless,
        None,
        &[],
        "aloecrypt/verify",
        args(vec![
            ("public_key", pk.as_str().into()),
            ("data", "m".into()),
            ("signature", sig),
        ]),
    )
    .unwrap();
    assert_eq!(get(&v, "valid").as_bool(), Some(true));
    assert!(call_in(
        &keyless,
        None,
        &[],
        "aloecrypt/hash",
        args(vec![("data", "m".into())])
    )
    .is_ok());
    for (name, alg) in [
        ("aloecrypt/sign", "ml-dsa-65"),
        ("aloecrypt/decrypt", "ml-kem-768"),
        ("aloecrypt/public_key", "ml-kem-768"),
    ] {
        let e = call_in(
            &keyless,
            None,
            &["*", "doc:*"],
            name,
            args(vec![
                ("key", "doc:1".into()),
                ("alg", alg.into()),
                ("data", "m".into()),
                ("document", "x".into()),
            ]),
        )
        .unwrap_err();
        assert!(e.contains("has no master key"), "{name}: {e}");
    }
}

#[test]
fn an_invalid_kem_key_is_refused_not_a_panic() {
    // A RECIPIENT document carrying the right length for ML-KEM-768 and
    // coefficients past the modulus. aloecrypt_core's encapsulator would
    // unwrap this key (aloecrypt_core#7); its reader refuses it instead.
    let bogus = MlKem768Encapsulator {
        public_key: [0xFFu8; MLKEM_768_ENCAP_KEY_SZ],
    };
    let mut doc = vec![0u8; public_key_document_size(MLKEM_768_ENCAP_KEY_SZ)];
    let n = write_recipient_768(&bogus, &mut doc).unwrap();
    let mut text = vec![0u8; armored_size(RECIPIENT_LABEL.len(), n)];
    let m = armor(RECIPIENT_LABEL, &doc[..n], &mut text).unwrap();
    let text = std::str::from_utf8(&text[..m]).unwrap();
    let e = call(
        &AloecryptConnector::new(),
        &[],
        "aloecrypt/encrypt",
        args(vec![("public_key", text.into()), ("data", "m".into())]),
    )
    .unwrap_err();
    assert!(e.contains("not a valid public-key document"), "{e}");
}

#[test]
fn malformed_input_is_named() {
    let c = AloecryptConnector::new();
    let pk = public_key(&c, "doc:1", "ml-dsa-65");
    let v = call(
        &c,
        &[],
        "aloecrypt/verify",
        args(vec![
            ("public_key", pk.as_str().into()),
            ("data", "m".into()),
            ("signature", "not armor".into()),
        ]),
    )
    .unwrap();
    assert_eq!(get(&v, "reason").as_str(), Some("malformed"));

    let e = call(
        &c,
        &[],
        "aloecrypt/verify",
        args(vec![
            ("public_key", "AAAA".into()),
            ("data", "m".into()),
            ("signature", "x".into()),
        ]),
    )
    .unwrap_err();
    assert!(
        e.contains("not an armored ALOECRYPT SIGNER or RECIPIENT document"),
        "{e}"
    );

    let kem = public_key(&c, "doc:1", "ml-kem-512");
    let e = call(
        &c,
        &[],
        "aloecrypt/verify",
        args(vec![
            ("public_key", kem.as_str().into()),
            ("data", "m".into()),
            ("signature", "x".into()),
        ]),
    )
    .unwrap_err();
    assert!(e.contains("ML-KEM key, which verifies nothing"), "{e}");
    let e = call(
        &c,
        &[],
        "aloecrypt/encrypt",
        args(vec![
            ("public_key", pk.as_str().into()),
            ("data", "m".into()),
        ]),
    )
    .unwrap_err();
    assert!(e.contains("ML-DSA key, which encrypts nothing"), "{e}");

    let e = call(
        &c,
        &["*"],
        "aloecrypt/sign",
        args(vec![
            ("key", "d".into()),
            ("alg", "ml-kem-768".into()),
            ("data", "m".into()),
        ]),
    )
    .unwrap_err();
    assert!(e.contains("does not sign"), "{e}");
    let e = call(
        &c,
        &[],
        "aloecrypt/public_key",
        args(vec![("key", "d".into())]),
    )
    .unwrap_err();
    assert!(e.contains("args.alg is required"), "{e}");
    let e = call(
        &c,
        &[],
        "aloecrypt/public_key",
        args(vec![("key", "d".into()), ("alg", "rsa".into())]),
    )
    .unwrap_err();
    assert!(e.contains("must be one of ml-dsa-44"), "{e}");
    let e = call(&c, &[], "aloecrypt/mint", args(vec![])).unwrap_err();
    assert!(e.contains("'aloecrypt/mint' is none of them"), "{e}");
}

#[test]
fn hash_is_the_cores_framed_keccak() {
    let c = AloecryptConnector::new();
    let hex = |a| {
        call(&c, &[], "aloecrypt/hash", a)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    };
    let expect = data_encoding::HEXLOWER.encode(&aloecrypt_core::hash::hash(
        b"",
        b"abc",
        DEFAULT_HASH_DOMAIN,
    ));
    assert_eq!(hex(args(vec![("data", "abc".into())])), expect);
    let expect = data_encoding::HEXLOWER.encode(&aloecrypt_core::hash::hash(b"s", b"abc", "d"));
    assert_eq!(
        hex(args(vec![
            ("data", "abc".into()),
            ("salt", "s".into()),
            ("domain", "d".into())
        ])),
        expect
    );
}

/// Interop: the signature document checks out under aloecrypt_core's
/// own verifier, built from the public key's bytes.
#[test]
fn a_signature_verifies_with_aloecrypt_core_directly() {
    let c = AloecryptConnector::new();
    let pk = public_key(&c, "doc:1", "ml-dsa-65");
    let sig = call(
        &c,
        &["doc:1"],
        "aloecrypt/sign",
        args(vec![("key", "doc:1".into()), ("data", "interop".into())]),
    )
    .unwrap();

    // The SIGNER document, read by the core's own reader.
    let mut doc = vec![0u8; pk.len()];
    let n = dearmor(pk.as_bytes(), SIGNER_LABEL, &mut doc).unwrap();
    let verifier = read_signer_65(&doc[..n]).unwrap();
    let text = sig.as_str().unwrap();
    let mut doc = vec![0u8; text.len()];
    let n = dearmor(text.as_bytes(), SIGNATURE_LABEL, &mut doc).unwrap();
    assert!(verify_detached_65(&verifier, b"interop", &doc[..n]).is_ok());
    assert!(verify_detached_65(&verifier, b"other", &doc[..n]).is_err());
}

#[test]
fn the_wiring_scope_is_checked_at_startup() {
    let validate = |v: rmpv::Value| {
        AloecryptConnector::new()
            .scope_type()
            .validate(Some(&Scope(v)))
    };
    assert!(
        validate(rmpv::Value::Map(vec![("key".into(), "short".into())]))
            .unwrap_err()
            .contains("shorter than 32")
    );
    assert!(validate(rmpv::Value::Map(vec![(
        "key_env".into(),
        "DRT_ALOECRYPT_TEST_UNSET_VAR".into()
    )]))
    .unwrap_err()
    .contains("is not set"));
    assert!(
        validate(rmpv::Value::Map(vec![("secret".into(), MASTER.into())]))
            .unwrap_err()
            .contains("unknown scope field")
    );
    assert!(validate(rmpv::Value::Map(vec![("key".into(), MASTER.into())])).is_ok());
    assert!(AloecryptConnector::new()
        .scope_type()
        .validate(None)
        .is_ok());
}

#[test]
fn ops_alone_round_trip() {
    // The plugin-facing half, with no msgpack in sight.
    let seed = ops::derive_seed(MASTER.as_bytes(), "x", ops::Alg::MlDsa44);
    let pk = ops::public_key(ops::Alg::MlDsa44, &seed);
    let decoded = ops::PublicKey::decode(&pk.encode()).unwrap();
    assert_eq!(decoded, pk);
    let sig = ops::sign(ops::Alg::MlDsa44, &seed, b"m").unwrap();
    assert_eq!(ops::verify(&decoded, b"m", &sig), Ok(()));
}

/// A page and wasip2 give the guest's drive loop wasm's default 1 MiB
/// stack, and ML-DSA is stack-hungry (aloecrypt_core DESIGN.md §9). Every
/// set's largest operation must fit in that, here on a thread that has no
/// more, in the unoptimized build that uses the most.
#[test]
fn every_operation_fits_in_a_one_mebibyte_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let master = MASTER.as_bytes();
            for alg in ops::Alg::ALL {
                let seed = ops::derive_seed(master, "stack", alg);
                let pk = ops::public_key(alg, &seed);
                if alg.signs() {
                    let sig = ops::sign(alg, &seed, b"m").unwrap();
                    assert_eq!(ops::verify(&pk, b"m", &sig), Ok(()));
                } else {
                    let doc = ops::encrypt(&pk, [1u8; 32], b"m").unwrap();
                    assert_eq!(ops::decrypt(alg, &seed, &doc).unwrap(), b"m");
                }
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
