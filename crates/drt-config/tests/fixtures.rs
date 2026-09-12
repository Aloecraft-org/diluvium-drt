//! The golden fixtures, asserted against the code that produces them.
//!
//! `tests/fixtures/` exists so drt and dollup compare against the same bytes
//! rather than each writing its own expectation; this file is what keeps
//! those bytes honest. A fixture that drifts from the implementation fails
//! here, in the repository that owns the format, rather than in whichever
//! consumer happens to notice second.
//!
//! Run with `DRT_WRITE_FIXTURES=1` to rewrite them after a deliberate format
//! change. Without it they are read and compared, which is what CI does.

use std::path::PathBuf;

use drt_caps::Grant;
use drt_config::canon;
use drt_config::consent::{Accepted, ConsentJson, Signer};
use drt_config::gsr::{Decision, Request, Verdict};
use drt_config::id::Uuid7;
use drt_config::project::{self, NodePath, ProjectJson};
use drt_config::realm::Realm;
use drt_config::sign::{Alg, KeyId, SecretKey};
use drt_config::time::{Timestamp, Window};

/// The test key, stated plainly so nobody has to wonder whether it is one.
const TEST_SEED: [u8; 32] = [5u8; 32];
const ROOT_ID: &str = "0192f0c1-8000-7000-8000-00000000abcd";
const CREATED: &str = "0192f0c1-8000-7000-8000-0000000012ef";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Compare, or rewrite when asked. One helper so every fixture is held to
/// the same rule.
fn golden(name: &str, produced: &str) {
    let path = dir().join(name);
    if std::env::var("DRT_WRITE_FIXTURES").is_ok() {
        std::fs::write(&path, produced).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run with DRT_WRITE_FIXTURES=1", path.display()));
    assert_eq!(
        on_disk.trim_end_matches('\n'),
        produced.trim_end_matches('\n'),
        "{} has drifted from the code",
        path.display()
    );
}

fn pretty<T: serde::Serialize>(value: &T) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}

fn root_id() -> Uuid7 {
    Uuid7::parse(ROOT_ID).unwrap()
}

fn accepted_at() -> Timestamp {
    Timestamp::parse("2026-09-11T20:14:00Z").unwrap()
}

fn ceiling() -> Vec<Grant> {
    vec![
        Grant::grant("host:fs/*"),
        Grant::grant("host:rest/get"),
        Grant::grant("host:time"),
        Grant::deny("host:fs/remove"),
    ]
}

fn project() -> ProjectJson {
    ProjectJson {
        project_name: Some("my_drt_project".into()),
        project_version: Some("0.0.0".into()),
        drt: Some("0.5.0".into()),
        caps: ceiling(),
        default_profile: Some("debug".into()),
        profiles: vec!["debug.config.json".into(), "preflight.config.json".into()],
        ..ProjectJson::new(root_id())
    }
}

fn request() -> Request {
    Request::new(
        Uuid7::parse(CREATED).unwrap(),
        root_id(),
        NodePath::parse("root/intake").unwrap(),
        Realm::parse("operator.rest.get").unwrap(),
        serde_json::json!({"add": ["example.com"]}),
    )
    .with_window(Window {
        from: None,
        until: Some(Timestamp::parse("2026-09-19T00:00:00Z").unwrap()),
    })
    .with_reason("the intake node needs the upstream")
}

/// The vector that prevents the "signature failures nobody can debug" class:
/// one input, the exact bytes, the exact digest. If dollup's canonicalizer
/// and drt's ever disagree, this is the first thing to compare.
#[test]
fn canonical_json_golden_vector() {
    let input = std::fs::read_to_string(dir().join("canonical.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&input).unwrap();
    let bytes = canon::to_canonical_bytes(&value);

    golden(
        "canonical.bytes",
        &String::from_utf8(bytes.clone()).unwrap(),
    );
    golden("canonical.sha256", canon::hash(&bytes).as_str());
}

#[test]
fn project_json_golden() {
    golden("project.json", &pretty(&project()));

    // And it reads back: a fixture that cannot be deserialized by the type it
    // came from is worse than no fixture.
    let text = std::fs::read_to_string(dir().join("project.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<ProjectJson>(&text).unwrap(),
        project()
    );
}

#[test]
fn consent_json_golden_in_both_modes() {
    let key = SecretKey::generate(TEST_SEED);
    let signers = vec![Signer {
        key_id: KeyId("portal-1".into()),
        alg: Alg::Ed25519,
        public_key: key.public_key(),
        realms: vec![Realm::parse("operator.rest").unwrap()],
    }];

    let listed = ConsentJson {
        root_id: root_id(),
        accepted: vec![Accepted::Listed {
            realm: Realm::root(),
            ceiling_hash: project::ceiling_hash(&project()).unwrap(),
            ceiling: ceiling(),
            accepted_at: accepted_at(),
        }],
        signers: signers.clone(),
    };
    let all = ConsentJson {
        root_id: root_id(),
        accepted: vec![Accepted::All {
            realm: Realm::root(),
            accepted_against: project::ceiling_hash(&project()).unwrap(),
            accepted_at: accepted_at(),
        }],
        signers,
    };

    golden("consent-listed.json", &pretty(&listed));
    golden("consent-all.json", &pretty(&all));

    for name in ["consent-listed.json", "consent-all.json"] {
        let text = std::fs::read_to_string(dir().join(name)).unwrap();
        serde_json::from_str::<ConsentJson>(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn gsr_request_and_decision_golden() {
    let key = SecretKey::generate(TEST_SEED);
    let request = request();
    let decision = Decision::sign(
        request.identity(),
        Verdict::Approve,
        Timestamp::parse("2026-09-13T00:00:00Z").unwrap(),
        KeyId("portal-1".into()),
        &key,
    )
    .unwrap();

    golden("gsr-request.json", &pretty(&request));
    golden("gsr-decision.json", &pretty(&decision));

    // The pending file's name is the identity hash, so the fixture records
    // it: a consumer that builds the same request must get the same filename
    // or its idempotency `stat` looks at the wrong path.
    golden("gsr-request.filename", &request.filename());

    // And the signature in the fixture verifies, which is the whole point of
    // shipping a signed example.
    let text = std::fs::read_to_string(dir().join("gsr-decision.json")).unwrap();
    let read: Decision = serde_json::from_str(&text).unwrap();
    assert_eq!(
        key.public_key()
            .verify(&read.signing_bytes().unwrap(), &read.signature),
        Ok(())
    );
    assert_eq!(read.request_hash, request.identity());
}
