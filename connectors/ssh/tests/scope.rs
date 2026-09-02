//! The ssh connector's first tests.
//!
//! It shipped in v0.3.1 unable to answer a single call — `tokio::time::timeout`
//! with no reactor under `pollster::block_on`, the same panic `rest` had —
//! and nothing noticed, because there was nothing to notice with. So the
//! first test here is the one that would have caught it, and it is deliberately
//! not an sshd test: it dials a closed port and requires an *error*. A panic
//! is a failure; a refused connection is a pass. That distinction is the
//! whole bug.
//!
//! What is still not covered, stated rather than implied: nothing here
//! authenticates, runs a command, or reads output. That needs a reachable
//! sshd and belongs in `doc/Verification.md` §2.1, which says so.

use drt_caps::{Scope, ScopeType};
use drt_connector::Connector;
use drt_connector_ssh::SshConnector;

/// A throwaway Ed25519 identity, generated per test from OS randomness.
/// Seeded rather than fixed on purpose: a constant seed is a committed
/// private key wearing a hat.
fn a_key() -> ssh_key::PrivateKey {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    ssh_key::private::Ed25519Keypair::from_seed(&seed).into()
}

/// `client_config` reads and parses the file, so the path has to lead to a
/// real key.
fn key_file(dir: &std::path::Path) -> std::path::PathBuf {
    let key = a_key();
    let path = dir.join("id_ed25519");
    std::fs::write(
        &path,
        key.to_openssh(ssh_key::LineEnding::LF).unwrap().as_bytes(),
    )
    .unwrap();
    path
}

/// A host key to trust. Any valid public key: nothing here gets far enough
/// to compare it against a server's.
fn a_host_key() -> String {
    a_key().public_key().to_openssh().unwrap()
}

fn scope_map(pairs: Vec<(&str, rmpv::Value)>) -> Scope {
    Scope(rmpv::Value::Map(
        pairs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    ))
}

fn good_scope(dir: &std::path::Path, host: &str) -> Scope {
    scope_map(vec![
        ("host", host.into()),
        ("user", "nobody".into()),
        (
            "key_path",
            key_file(dir).to_str().unwrap().to_string().into(),
        ),
        ("host_key", a_host_key().into()),
        ("timeout_ms", rmpv::Value::from(1500u64)),
    ])
}

/// A port nothing is listening on: bind one, read the number, drop it.
fn a_closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

/// **The regression test for the v0.3.1 bug.**
///
/// `drt run` drives connectors under `pollster::block_on`, which carries no
/// tokio reactor, and every socket call needs one. The connector answered
/// `there is no reactor running` and exit 101 — a panic, not a refusal, so
/// no amount of checking the reply would have found it.
///
/// The assertion is therefore about *how* it fails: dialing a closed port
/// must produce an `Err`. Reaching this line at all means it did not panic.
#[test]
fn a_call_with_no_reactor_refuses_rather_than_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let c = SshConnector::new();
    let sc = good_scope(dir.path(), &format!("127.0.0.1:{}", a_closed_port()));
    let args = rmpv::Value::Map(vec![("command".into(), "true".into())]);

    let out = pollster::block_on(c.call("ssh/exec", Some(args), Some(&sc)));
    let err = out.expect_err("nothing is listening; this cannot succeed");
    assert!(
        !err.to_string().is_empty(),
        "a refusal has to say something"
    );
}

/// And the same call under a real runtime, which is `drt start`'s shape. Both
/// paths exist and both must reach the socket rather than the panic.
#[test]
fn a_call_inside_a_runtime_refuses_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let c = SshConnector::new();
    let sc = good_scope(dir.path(), &format!("127.0.0.1:{}", a_closed_port()));
    let args = rmpv::Value::Map(vec![("command".into(), "true".into())]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(c.call("ssh/exec", Some(args), Some(&sc)));
    out.expect_err("nothing is listening; this cannot succeed");
}

/// The scope is validated at startup, by name. Each of these is a deployment
/// that would otherwise fail at 3am as an auth error.
#[test]
fn an_ill_formed_scope_is_refused_at_startup_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let ty = SshConnector::new().scope_type();
    let key = key_file(dir.path()).to_str().unwrap().to_string();

    assert!(ty.validate(None).is_err(), "no scope at all");

    let no_anchor = scope_map(vec![
        ("host", "h:22".into()),
        ("user", "u".into()),
        ("key_path", key.clone().into()),
    ]);
    let why = ty.validate(Some(&no_anchor)).unwrap_err();
    assert!(
        why.contains("trust") && why.contains("host_key"),
        "trust-on-first-use is never the default, and the refusal says so: {why}"
    );

    let missing_key = scope_map(vec![
        ("host", "h:22".into()),
        ("user", "u".into()),
        ("key_path", "/nonexistent/id_ed25519".into()),
        ("host_key", a_host_key().into()),
    ]);
    let why = ty.validate(Some(&missing_key)).unwrap_err();
    assert!(
        why.contains("cannot read key file"),
        "a bad key path is named at boot: {why}"
    );

    let zero_timeout = scope_map(vec![
        ("host", "h:22".into()),
        ("user", "u".into()),
        ("key_path", key.clone().into()),
        ("host_key", a_host_key().into()),
        ("timeout_ms", rmpv::Value::from(0u64)),
    ]);
    assert!(
        ty.validate(Some(&zero_timeout)).is_err(),
        "a timeout of 0 refuses every call; refuse the wiring instead"
    );

    let empty_user = scope_map(vec![
        ("host", "h:22".into()),
        ("user", "".into()),
        ("key_path", key.into()),
        ("host_key", a_host_key().into()),
    ]);
    assert!(ty.validate(Some(&empty_user)).is_err(), "empty user");
}

/// A well-formed scope passes the same gate, so the test above is measuring
/// the refusals rather than a validator that refuses everything.
#[test]
fn a_well_formed_scope_validates() {
    let dir = tempfile::tempdir().unwrap();
    let ty = SshConnector::new().scope_type();
    ty.validate(Some(&good_scope(dir.path(), "example.invalid:22")))
        .unwrap();
}

/// An unknown verb in the family is refused rather than silently ignored.
#[test]
fn an_unknown_call_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let c = SshConnector::new();
    let sc = good_scope(dir.path(), "example.invalid:22");
    pollster::block_on(c.call("ssh/nonsense", None, Some(&sc))).expect_err("only ssh/exec exists");
}
