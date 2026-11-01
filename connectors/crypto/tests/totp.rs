//! `crypto/totp`, `crypto/totp_verify` and `crypto/totp_enroll` at the
//! connector boundary, against `totp-rs`, an implementation that shares no
//! code with this crate. The RFC 6238 table itself is checked against the
//! arithmetic in `src/lib.rs` (`mod vectors`), since these calls read the
//! host clock and a test cannot set it.
//!
//! # Surface
//!
//! Entry points: the tests.
//!
//! Configurable values:
//! - `DEV_KEY`, the master.
//! - `RFC_SEED_SHA1`, `RFC_SEED_SHA256`: RFC 6238 Appendix B's seeds,
//!   wired as the configured secrets `rfc-sha1` and `rfc-sha256`.
//!
//! Fan-out: none.

use std::sync::Arc;

use drt_caps::{CapSet, Grant, Scope};
use drt_connector::{Asker, Caller, Connector, Dispatcher, Registry};
use drt_connector_crypto::{CryptoConnector, KDF_LABEL_DERIVE, KDF_LABEL_TOTP};
use drt_hostcall::{to_bytes, Request, Status};
use hmac::{Mac, SimpleHmac};
use sha2::Sha256;
use totp_rs::{Algorithm, Builder, Totp};

const DEV_KEY: &str = "capability-testing-dev-key-0123456789";
const RFC_SEED_SHA1: &str = "12345678901234567890";
const RFC_SEED_SHA256: &str = "12345678901234567890123456789012";

// depth: the scope, the calls, and the independent implementation

fn map(entries: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn scope() -> Scope {
    let secret = |name: &str, key: &str| map(vec![("name", name.into()), ("key", key.into())]);
    Scope(map(vec![
        ("key", DEV_KEY.into()),
        (
            "secrets",
            rmpv::Value::Array(vec![
                secret("rfc-sha1", RFC_SEED_SHA1),
                secret("rfc-sha256", RFC_SEED_SHA256),
            ]),
        ),
    ]))
}

const NODE: Caller = Caller::Node(1);

fn call_as(
    c: &CryptoConnector,
    caller: Caller,
    name: &str,
    a: rmpv::Value,
) -> Result<rmpv::Value, String> {
    let grants = [Scope(rmpv::Value::from("totp:*"))];
    let asker = Asker {
        caller,
        grants: &grants,
    };
    pollster::block_on(c.call_as(&asker, name, Some(a), Some(&scope()))).map_err(|e| e.to_string())
}

fn call(c: &CryptoConnector, name: &str, a: rmpv::Value) -> Result<rmpv::Value, String> {
    call_as(c, NODE, name, a)
}

fn derive(c: &CryptoConnector, label: &str) {
    call(c, "crypto/derive", map(vec![("label", label.into())])).expect("derive");
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn reference(alg: Algorithm, digits: u8, period: u64, seed: &[u8]) -> Totp {
    Builder::new()
        .with_algorithm(alg)
        .with_digits(digits)
        .with_step_duration(period)
        .with_secret(seed.to_vec())
        .with_account_name("test")
        .build()
        .unwrap()
}

/// Run `body` with the step it is handed, and again if the clock crossed
/// into the next step while it ran: a test that reads the step and then
/// computes codes around it must not straddle a boundary. A failure that
/// happened while the step moved is retried; one inside a single step is
/// the failure.
fn within_one_step(period: u64, body: impl Fn(u64)) {
    for _ in 0..3 {
        let before = now() / period;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(before)));
        if now() / period == before {
            if let Err(panic) = outcome {
                std::panic::resume_unwind(panic);
            }
            return;
        }
    }
    panic!("the clock crossed a step boundary three times running");
}

fn verify(
    c: &CryptoConnector,
    mut a: Vec<(&str, rmpv::Value)>,
    key: &str,
    code: &str,
) -> rmpv::Value {
    a.push(("key", key.into()));
    a.push(("code", code.into()));
    call(c, "crypto/totp_verify", map(a)).expect("a verdict is an answer")
}

fn hmac256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = <SimpleHmac<Sha256> as Mac>::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// Agreement with an independent implementation
// ---------------------------------------------------------------------------

/// The RFC seeds as configured secrets, both hashes, six and eight digits,
/// two periods: the code this host answers is the one `totp-rs` computes
/// for the step this host says it belongs to, and `expires` is that step's
/// end.
#[test]
fn the_current_code_matches_totp_rs() {
    let c = CryptoConnector::new();
    for (key, alg, name, seed) in [
        ("rfc-sha1", "sha1", Algorithm::SHA1, RFC_SEED_SHA1),
        ("rfc-sha256", "sha256", Algorithm::SHA256, RFC_SEED_SHA256),
    ] {
        for digits in [6u8, 8] {
            for period in [30u64, 60] {
                let got = call(
                    &c,
                    "crypto/totp",
                    map(vec![
                        ("key", key.into()),
                        ("alg", alg.into()),
                        ("digits", (digits as i64).into()),
                        ("period", (period as i64).into()),
                    ]),
                )
                .unwrap();
                let step = field(&got, "step").as_u64().unwrap();
                let t = now();
                assert!(
                    step == t / period || step + 1 == t / period,
                    "step {step} at {t}"
                );
                assert_eq!(field(&got, "expires").as_u64(), Some((step + 1) * period));
                let want = reference(name, digits, period, seed.as_bytes())
                    .generate(step * period)
                    .to_string();
                assert_eq!(
                    field(&got, "code").as_str(),
                    Some(want.as_str()),
                    "{key} {digits} {period}"
                );
            }
        }
    }
}

/// The defaults are what an authenticator assumes when an enrolment names
/// nothing: SHA-1, six digits, thirty seconds.
#[test]
fn the_defaults_are_sha1_six_digits_thirty_seconds() {
    let c = CryptoConnector::new();
    let got = call(&c, "crypto/totp", map(vec![("key", "rfc-sha1".into())])).unwrap();
    let step = field(&got, "step").as_u64().unwrap();
    let want = reference(Algorithm::SHA1, 6, 30, RFC_SEED_SHA1.as_bytes())
        .generate(step * 30)
        .to_string();
    assert_eq!(field(&got, "code").as_str(), Some(want.as_str()));
}

/// A code `totp-rs` computes now verifies here, under both hashes.
#[test]
fn a_code_from_totp_rs_verifies() {
    let c = CryptoConnector::new();
    for (key, alg, name, seed) in [
        ("rfc-sha1", "sha1", Algorithm::SHA1, RFC_SEED_SHA1),
        ("rfc-sha256", "sha256", Algorithm::SHA256, RFC_SEED_SHA256),
    ] {
        within_one_step(30, |step| {
            let code = reference(name, 8, 30, seed.as_bytes())
                .generate(step * 30)
                .to_string();
            let v = verify(
                &c,
                vec![("alg", alg.into()), ("digits", 8.into())],
                key,
                &code,
            );
            assert!(valid(&v), "{key}: {v:?}");
            assert_eq!(field(&v, "step").as_u64(), Some(step));
        });
    }
}

/// Enrolment end to end, as a person would: the URI goes into an
/// authenticator (here `totp-rs` parsing it), the authenticator shows a
/// code, the code verifies here. The URI carries every parameter, so the
/// authenticator and the host agree on what it means.
#[test]
fn an_enrolled_uri_drives_an_authenticator_whose_codes_verify() {
    let c = CryptoConnector::new();
    derive(&c, "totp:alice");
    for (alg, name, digits, period) in [
        ("sha1", Algorithm::SHA1, 6u8, 30u64),
        ("sha256", Algorithm::SHA256, 8, 60),
    ] {
        let e = call(
            &c,
            "crypto/totp_enroll",
            map(vec![
                ("key", "totp:alice".into()),
                ("account", "alice@example.com".into()),
                ("issuer", "Acme Corp".into()),
                ("alg", alg.into()),
                ("digits", (digits as i64).into()),
                ("period", (period as i64).into()),
            ]),
        )
        .unwrap();
        let uri = field(&e, "uri").as_str().unwrap();
        let secret = field(&e, "secret").as_str().unwrap();
        assert!(
            uri.starts_with("otpauth://totp/Acme%20Corp:alice%40example.com?secret="),
            "{uri}"
        );
        let app = Totp::from_url(uri).expect("totp-rs reads the URI");
        assert_eq!(app.algorithm(), name);
        assert_eq!(app.digits(), digits);
        assert_eq!(app.step(), period);
        assert_eq!(app.issuer(), Some("Acme Corp"));
        assert_eq!(app.account_name(), "alice@example.com");
        assert_eq!(app.secret().to_base32(), secret);
        // RFC 6238's seed lengths: the hash's output size.
        assert_eq!(
            app.secret().as_bytes().len(),
            if alg == "sha1" { 20 } else { 32 }
        );

        within_one_step(period, |step| {
            let code = app.generate(step * period).to_string();
            let v = verify(
                &c,
                vec![
                    ("alg", alg.into()),
                    ("digits", (digits as i64).into()),
                    ("period", (period as i64).into()),
                ],
                "totp:alice",
                &code,
            );
            assert!(valid(&v), "{alg}: {v:?}");
        });
    }
}

/// A label's seed is `HMAC(label master, KDF_LABEL_TOTP)`, truncated,
/// computed here from the public labels: an enrolled authenticator keeps
/// working across a restart and on any host with the master.
#[test]
fn a_labels_seed_is_pinned_to_the_public_kdf() {
    let c = CryptoConnector::new();
    derive(&c, "totp:alice");
    let k_label = hmac256(DEV_KEY.as_bytes(), KDF_LABEL_DERIVE);
    let label_master = hmac256(&k_label, b"totp:alice");
    let seed = hmac256(&label_master, KDF_LABEL_TOTP);
    for (alg, len) in [("sha1", 20), ("sha256", 32)] {
        let e = call(
            &c,
            "crypto/totp_enroll",
            map(vec![
                ("key", "totp:alice".into()),
                ("account", "alice".into()),
                ("alg", alg.into()),
            ]),
        )
        .unwrap();
        let got = totp_rs::Secret::try_from_base32(field(&e, "secret").as_str().unwrap()).unwrap();
        assert_eq!(got.as_bytes(), &seed[..len]);
    }
    // Two labels, two seeds; and no issuer leaves the label bare.
    derive(&c, "totp:bob");
    let a = call(
        &c,
        "crypto/totp_enroll",
        map(vec![("key", "totp:alice".into()), ("account", "a".into())]),
    )
    .unwrap();
    let b = call(
        &c,
        "crypto/totp_enroll",
        map(vec![("key", "totp:bob".into()), ("account", "a".into())]),
    )
    .unwrap();
    assert_ne!(field(&a, "secret"), field(&b, "secret"));
    assert!(field(&a, "uri")
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/a?secret="));
    assert!(!field(&a, "uri").as_str().unwrap().contains("issuer="));
}

// ---------------------------------------------------------------------------
// The window, replay, and malformed codes
// ---------------------------------------------------------------------------

/// `window` steps either side of now and no further: the default is one.
#[test]
fn the_window_bounds_how_far_a_code_may_drift() {
    let c = CryptoConnector::new();
    let app = reference(Algorithm::SHA1, 6, 30, RFC_SEED_SHA1.as_bytes());
    within_one_step(30, |step| {
        let at = |s: u64| app.generate(s * 30).to_string();
        // A code may by chance equal another step's; skip a sample where
        // the reference codes collide, rather than assert on luck.
        let codes: Vec<String> = (step - 3..=step + 3).map(at).collect();
        let distinct = codes
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == codes.len();
        if !distinct {
            return;
        }
        for (offset, default_ok) in [(-2i64, false), (-1, true), (0, true), (1, true), (2, false)] {
            let s = (step as i64 + offset) as u64;
            let v = verify(&c, vec![], "rfc-sha1", &at(s));
            assert_eq!(valid(&v), default_ok, "offset {offset}: {v:?}");
            if !default_ok {
                assert_eq!(reason(&v), "mismatch");
            }
            let wide = verify(&c, vec![("window", 2.into())], "rfc-sha1", &at(s));
            assert!(valid(&wide), "offset {offset} under window 2");
            assert_eq!(field(&wide, "step").as_u64(), Some(s));
            let none = verify(&c, vec![("window", 0.into())], "rfc-sha1", &at(s));
            assert_eq!(valid(&none), offset == 0, "offset {offset} under window 0");
        }
    });
}

/// RFC 6238 §5.2: a code accepted once is not accepted again. The caller
/// keeps the answered step and passes it back as `after`.
#[test]
fn after_refuses_a_code_already_used() {
    let c = CryptoConnector::new();
    let app = reference(Algorithm::SHA1, 6, 30, RFC_SEED_SHA1.as_bytes());
    within_one_step(30, |step| {
        let code = app.generate(step * 30).to_string();
        let first = verify(&c, vec![], "rfc-sha1", &code);
        assert!(valid(&first));
        let used = field(&first, "step").as_u64().unwrap();
        let again = verify(&c, vec![("after", used.into())], "rfc-sha1", &code);
        assert!(!valid(&again));
        assert_eq!(reason(&again), "replayed");
        // An older code is refused the same way, and the next one is not.
        let older = app.generate((step - 1) * 30).to_string();
        if older != code {
            assert_eq!(
                reason(&verify(
                    &c,
                    vec![("after", used.into())],
                    "rfc-sha1",
                    &older
                )),
                "replayed"
            );
        }
        let next = app.generate((step + 1) * 30).to_string();
        let v = verify(&c, vec![("after", used.into())], "rfc-sha1", &next);
        assert!(valid(&v), "{v:?}");
        assert_eq!(field(&v, "step").as_u64(), Some(step + 1));
        // `after` below the step changes nothing.
        assert!(valid(&verify(
            &c,
            vec![("after", (used - 5).into())],
            "rfc-sha1",
            &code
        )));
    });
}

/// A code is `digits` decimal digits. An integer is taken and zero-padded,
/// because a Lua program that read the code as a number lost only its
/// leading zeros.
#[test]
fn a_code_is_digits_and_an_integer_is_padded() {
    let c = CryptoConnector::new();
    let app = reference(Algorithm::SHA1, 6, 30, RFC_SEED_SHA1.as_bytes());
    within_one_step(30, |step| {
        let code = app.generate(step * 30).to_string();
        let n: u64 = code.parse().unwrap();
        let v = call(
            &c,
            "crypto/totp_verify",
            map(vec![("key", "rfc-sha1".into()), ("code", n.into())]),
        )
        .unwrap();
        assert!(valid(&v), "{code} as {n}: {v:?}");
    });
    for bad in ["12345", "1234567", "12345a", " 123456", "", "１２３４５６"] {
        assert_eq!(
            reason(&verify(&c, vec![], "rfc-sha1", bad)),
            "malformed",
            "{bad:?}"
        );
    }
    let err = call(
        &c,
        "crypto/totp_verify",
        map(vec![
            ("key", "rfc-sha1".into()),
            ("code", rmpv::Value::Boolean(true)),
        ]),
    )
    .unwrap_err();
    assert!(
        err.contains("args.code must be a string of digits"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// Refusals: the key, the parameters, the grant
// ---------------------------------------------------------------------------

/// A configured secret checks codes and is never enrolled: those bytes
/// were provisioned elsewhere and are not handed to a program.
#[test]
fn enroll_refuses_a_configured_secret_and_an_underived_label() {
    let c = CryptoConnector::new();
    let err = call(
        &c,
        "crypto/totp_enroll",
        map(vec![("key", "rfc-sha1".into()), ("account", "a".into())]),
    )
    .unwrap_err();
    assert!(err.contains("never handed to a program"), "{err}");
    let err = call(
        &c,
        "crypto/totp_enroll",
        map(vec![("key", "totp:nobody".into()), ("account", "a".into())]),
    )
    .unwrap_err();
    assert!(
        err.contains("derived no label named 'totp:nobody'"),
        "{err}"
    );
}

/// A label is the caller's: another node holding the same grant has not
/// derived it, so the name reaches nothing for it.
#[test]
fn a_label_is_the_callers() {
    let c = CryptoConnector::new();
    derive(&c, "totp:alice");
    let err = call_as(
        &c,
        Caller::Node(2),
        "crypto/totp",
        map(vec![("key", "totp:alice".into())]),
    )
    .unwrap_err();
    assert!(err.contains("derived no label named 'totp:alice'"), "{err}");
    // Released with its owner, and the same seed when it is derived again.
    let before = call(&c, "crypto/totp", map(vec![("key", "totp:alice".into())])).unwrap();
    c.release(&NODE);
    assert!(call(&c, "crypto/totp", map(vec![("key", "totp:alice".into())])).is_err());
    derive(&c, "totp:alice");
    let after = call(&c, "crypto/totp", map(vec![("key", "totp:alice".into())])).unwrap();
    if field(&before, "step") == field(&after, "step") {
        assert_eq!(field(&before, "code"), field(&after, "code"));
    }
}

#[test]
fn parameters_out_of_range_are_refused_naming_the_range() {
    let c = CryptoConnector::new();
    for (name, value, expect) in [
        (
            "digits",
            rmpv::Value::from(5),
            "args.digits must be an integer 6..8",
        ),
        (
            "digits",
            rmpv::Value::from(9),
            "args.digits must be an integer 6..8",
        ),
        (
            "period",
            rmpv::Value::from(5),
            "args.period must be an integer 10..300",
        ),
        (
            "period",
            rmpv::Value::from(301),
            "args.period must be an integer 10..300",
        ),
        (
            "period",
            "30".into(),
            "args.period must be an integer 10..300",
        ),
        ("alg", "sha512".into(), "'sha1' (the default) or 'sha256'"),
        ("alg", "SHA1".into(), "'sha1' (the default) or 'sha256'"),
    ] {
        for verb in ["crypto/totp", "crypto/totp_verify"] {
            let err = call(
                &c,
                verb,
                map(vec![
                    ("key", "rfc-sha1".into()),
                    ("code", "123456".into()),
                    (name, value.clone()),
                ]),
            )
            .unwrap_err();
            assert!(err.contains(expect), "{verb} {name}: {err}");
        }
    }
    for (name, value, expect) in [
        (
            "window",
            rmpv::Value::from(11),
            "args.window must be an integer 0..10",
        ),
        (
            "window",
            rmpv::Value::from(-1),
            "args.window must be an integer 0..10",
        ),
        ("after", rmpv::Value::from(-1), "args.after must be a step"),
    ] {
        let err = call(
            &c,
            "crypto/totp_verify",
            map(vec![
                ("key", "rfc-sha1".into()),
                ("code", "123456".into()),
                (name, value),
            ]),
        )
        .unwrap_err();
        assert!(err.contains(expect), "{name}: {err}");
    }
    let err = call(&c, "crypto/totp", map(vec![])).unwrap_err();
    assert!(err.contains("args.key must be a string"), "{err}");
}

#[test]
fn enroll_names_are_bounded_and_carry_no_colon() {
    let c = CryptoConnector::new();
    derive(&c, "totp:alice");
    let long = "x".repeat(129);
    for (account, issuer) in [
        (None, None),
        (Some(""), None),
        (Some("a:b"), None),
        (Some(long.as_str()), None),
        (Some("alice"), Some("Ac:me")),
        (Some("alice"), Some("")),
    ] {
        let mut a = vec![("key", "totp:alice".into())];
        if let Some(account) = account {
            a.push(("account", account.into()));
        }
        if let Some(issuer) = issuer {
            a.push(("issuer", issuer.into()));
        }
        let err = call(&c, "crypto/totp_enroll", map(a)).unwrap_err();
        assert!(
            err.contains("1..128 bytes with no ':'"),
            "{account:?} {issuer:?}: {err}"
        );
    }
}

/// Three verbs, three grants: the program that checks codes holds
/// `totp_verify` and never the one that answers a seed.
#[test]
fn enroll_and_generate_are_separate_grants_from_verify() {
    let mut reg = Registry::new();
    reg.wire("crypto", Arc::new(CryptoConnector::new()), Some(scope()))
        .unwrap();
    let d = Dispatcher::new(reg);
    let caps = CapSet::root(vec![Grant::grant("host:crypto/totp_verify")]);
    let request = |tok, call: &str, a| {
        to_bytes(&Request {
            tok,
            call: call.into(),
            args: Some(a),
        })
        .unwrap()
    };
    let verify = request(
        1,
        "crypto/totp_verify",
        map(vec![("key", "rfc-sha1".into()), ("code", "000000".into())]),
    );
    assert_eq!(
        pollster::block_on(d.dispatch(&caps, &verify)).status,
        Status::Ok
    );
    for (tok, verb) in [(2, "crypto/totp"), (3, "crypto/totp_enroll")] {
        let r = request(
            tok,
            verb,
            map(vec![("key", "rfc-sha1".into()), ("account", "a".into())]),
        );
        assert_eq!(
            pollster::block_on(d.dispatch(&caps, &r)).status,
            Status::Denied,
            "{verb}"
        );
    }
}
