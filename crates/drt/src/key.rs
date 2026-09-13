//! `drt key new` and `drt key sign`: the cryptography, without dollup.
//!
//! drt does the signing and never requires dollup to be installed. dollup
//! stores keys, lists them and copies public keys into a root's `consent.json`;
//! anything it can do to a key, this can do to the same key with a path
//! argument.
//!
//! `key sign` is what makes consent.md §8's promise real rather than
//! theoretical. The runtime writes pending requests and reads decisions from
//! directories and cannot tell a portal behind an auth proxy from a human with
//! a text editor — but "a human with a text editor" is only true if a human can
//! actually produce a valid signature. This is that, in one command.
//!
//! ## surface block
//!
//! - Entry points: [`new`], generate and write a key; [`sign`], approve or deny
//!   a pending request.
//! - Configurable values: [`DEFAULT_HOURS`], how long an approval lasts when
//!   nobody says; [`KEY_MODE`], the permission a key file is written with.
//! - Fan-out: none. Two commands, no dispatch.

use std::path::Path;

use drt_config::gsr::{Decision, Request, Verdict};
use drt_config::sign::{KeyId, SecretKey};
use drt_config::time::Timestamp;

/// How long an approval lasts when `--not-after` is not given.
///
/// Short on purpose. `not_after` is the real lifetime knob because revocation
/// is out of scope (consent.md §7), so the default has to be a length somebody
/// would be comfortable with having forgotten about. An hour is that; a week is
/// not.
pub const DEFAULT_HOURS: i64 = 1;

/// `0600`. A signing key readable by anything else on the box is not a signing
/// key.
#[cfg(unix)]
pub const KEY_MODE: u32 = 0o600;

/// Generate a key, write its seed, and print the public half.
///
/// The public key is what goes into a root's `consent.json` `signers` list, so
/// it goes to **stdout** and everything else goes to stderr: `drt key new k >
/// k.pub` should produce a file with one key in it.
pub fn new(path: &Path, out: &mut dyn std::io::Write) -> Result<(), String> {
    if drt_platform::fs::exists(path) {
        // Never silently. Overwriting a signing key is losing every signature
        // it ever made the ability to be re-made.
        return Err(format!(
            "{} already exists; a key file is never overwritten",
            path.display()
        ));
    }
    let mut seed = [0u8; 32];
    drt_platform::entropy::fill(&mut seed)
        .map_err(|e| format!("cannot generate a key: no entropy source ({e})"))?;
    let key = SecretKey::generate(seed);

    // The stored form is `drt_config::sign`'s, not this file's: dollup reads the
    // same key out of `~/.dollup/keys/`, and two implementations of one format
    // are two formats that agree until they do not.
    drt_platform::fs::write(path, key.to_file_text())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    restrict(path)?;

    let public: String = key.public_key().into();
    writeln!(out, "{public}").map_err(|e| e.to_string())?;
    eprintln!(
        "wrote {} (the private seed; keep it), and printed its public key",
        path.display()
    );
    Ok(())
}

/// Sign a decision about a pending request.
///
/// `request` is a file in `state/gsr/pending/`. The decision is written beside
/// it in `state/gsr/decided/`, named after the request so a reader can pair
/// them by eye — the runtime does not care about the name and reads every file
/// in there, which is what keeps the directory somebody else's to write.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    key_path: &Path,
    request_path: &Path,
    decided_dir: &Path,
    key_id: &str,
    verdict: Verdict,
    not_after: Option<Timestamp>,
    now: Timestamp,
) -> Result<std::path::PathBuf, String> {
    let key = load(key_path)?;
    let text = drt_platform::fs::read_to_string(request_path)
        .map_err(|e| format!("cannot read {}: {e}", request_path.display()))?;
    let request: Request = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not a grant request: {e}", request_path.display()))?;

    let not_after = not_after
        .unwrap_or_else(|| Timestamp::from_unix_secs(now.unix_secs() + DEFAULT_HOURS * 3_600));
    if !not_after.is_after(now) {
        // A decision that is already expired verifies and then fails step 4,
        // which reads as a mystery. Refused where it is written instead.
        return Err(format!(
            "--not-after {not_after} is not in the future; the decision would expire before it \
             could be read"
        ));
    }

    let decision = Decision::sign(
        request.identity(),
        verdict,
        not_after,
        KeyId(key_id.to_string()),
        &key,
    )
    .map_err(|e| format!("cannot sign: {e}"))?;

    drt_platform::fs::create_dir_all(decided_dir)
        .map_err(|e| format!("cannot create {}: {e}", decided_dir.display()))?;
    let name = request_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("decision.json");
    let path = decided_dir.join(name);
    let body = serde_json::to_string_pretty(&decision)
        .map_err(|e| format!("cannot serialize the decision: {e}"))?;
    drt_platform::fs::write(&path, format!("{body}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

// depth: the key file, and the one place this reaches past the platform layer

/// Read a seed and rebuild the key, naming the file in the refusal.
fn load(path: &Path) -> Result<SecretKey, String> {
    let text = drt_platform::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    SecretKey::from_file_text(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Take the permissions down to [`KEY_MODE`].
///
/// The one place this crate reaches past `drt_platform::fs` to `std::fs`, and
/// the reason is that a file mode is not a thing a memory filesystem has: a
/// page has no other process to hide a key from, and a backend method for it
/// would be a method with one real implementation and one no-op. On a target
/// without modes this is a no-op and says so, rather than pretending.
fn restrict(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Only when the file is really on a disk. Under a `MemFs` -- every test
        // here, and a page -- there is nothing to chmod and `std::fs` would
        // answer "not found" for a path that exists as far as the program is
        // concerned.
        if std::fs::metadata(path).is_ok() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(KEY_MODE))
                .map_err(|e| format!("cannot restrict {} to 0600: {e}", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drt_config::consent::{Accepted, ConsentJson, Signer};
    use drt_config::gsr::{self, Verified};
    use drt_config::project::{NodePath, ProjectJson};
    use drt_config::realm::{Realm, RealmRegistry};
    use drt_config::sign::Alg;

    use crate::testfs::{self, Seeded};

    fn now() -> Timestamp {
        Timestamp::parse("2026-09-12T00:00:00Z").unwrap()
    }

    fn request() -> Request {
        Request::new(
            drt_config::id::Uuid7::parse("0192f0c1-8000-7000-8000-0000000012ef").unwrap(),
            Seeded::root_id(),
            NodePath::parse("root/intake").unwrap(),
            Realm::parse("operator.rest.get").unwrap(),
            serde_json::json!({"add": ["example.com"]}),
        )
    }

    /// The whole loop with nothing but drt: generate a key, put its public half
    /// in `consent.json`, sign the pending request, and have the four-step
    /// chain accept it. This is what "a human with a text editor" has to mean
    /// in order to be true.
    #[test]
    fn a_key_made_here_signs_a_decision_the_runtime_accepts() {
        let seeded = testfs::seed(true, &[]);
        let key_path = std::path::PathBuf::from("/r/portal-1.key");

        let mut printed = Vec::new();
        new(&key_path, &mut printed).unwrap();
        let public: drt_config::sign::PublicKey = serde_json::from_str(&format!(
            "\"{}\"",
            String::from_utf8(printed).unwrap().trim()
        ))
        .expect("the printed key parses as one");

        // The pending request, as the runtime would have written it.
        let request = request();
        seeded.root.ensure_state().unwrap();
        let pending = seeded.root.gsr_pending().join(request.filename());
        drt_platform::fs::write(&pending, serde_json::to_string_pretty(&request).unwrap()).unwrap();

        let written = sign(
            &key_path,
            &pending,
            &seeded.root.gsr_decided(),
            "portal-1",
            Verdict::Approve,
            None,
            now(),
        )
        .unwrap();

        // And it verifies, end to end, against a consent file naming that key.
        let ceiling = vec![drt_caps::Grant::grant("host:rest/get")];
        let project = ProjectJson {
            caps: ceiling.clone(),
            ..ProjectJson::new(Seeded::root_id())
        };
        let consent = ConsentJson {
            root_id: Seeded::root_id(),
            accepted: vec![Accepted::Listed {
                realm: Realm::root(),
                ceiling_hash: drt_config::project::ceiling_hash(&project).unwrap(),
                ceiling: drt_config::project::DeclaredCeiling::of_caps(ceiling),
                accepted_at: now(),
            }],
            signers: vec![Signer {
                key_id: KeyId("portal-1".into()),
                alg: Alg::Ed25519,
                public_key: public,
                realms: vec![Realm::root()],
            }],
            peers: Vec::new(),
        };
        let decision: Decision =
            serde_json::from_str(&drt_platform::fs::read_to_string(&written).unwrap()).unwrap();
        let verified =
            gsr::verify(&request, &decision, &consent, &RealmRegistry::new(), now()).unwrap();
        assert!(matches!(verified, Verified::Granted { .. }), "{verified:?}");
    }

    /// The default lifetime is short, because revocation is out of scope and a
    /// forgotten approval is the failure mode.
    #[test]
    fn the_default_lifetime_is_an_hour() {
        let seeded = testfs::seed(true, &[]);
        let key_path = std::path::PathBuf::from("/r/k");
        new(&key_path, &mut Vec::new()).unwrap();

        let request = request();
        seeded.root.ensure_state().unwrap();
        let pending = seeded.root.gsr_pending().join(request.filename());
        drt_platform::fs::write(&pending, serde_json::to_string(&request).unwrap()).unwrap();

        let written = sign(
            &key_path,
            &pending,
            &seeded.root.gsr_decided(),
            "portal-1",
            Verdict::Approve,
            None,
            now(),
        )
        .unwrap();
        let decision: Decision =
            serde_json::from_str(&drt_platform::fs::read_to_string(&written).unwrap()).unwrap();
        assert_eq!(
            decision.not_after.unix_secs() - now().unix_secs(),
            DEFAULT_HOURS * 3_600
        );
    }

    /// A decision that is already expired would verify and then fail step 4,
    /// which reads as a mystery. Refused where it is written.
    #[test]
    fn an_already_expired_decision_is_refused_at_signing_time() {
        let seeded = testfs::seed(true, &[]);
        let key_path = std::path::PathBuf::from("/r/k");
        new(&key_path, &mut Vec::new()).unwrap();

        let request = request();
        seeded.root.ensure_state().unwrap();
        let pending = seeded.root.gsr_pending().join(request.filename());
        drt_platform::fs::write(&pending, serde_json::to_string(&request).unwrap()).unwrap();

        let e = sign(
            &key_path,
            &pending,
            &seeded.root.gsr_decided(),
            "portal-1",
            Verdict::Approve,
            Some(Timestamp::parse("2020-01-01T00:00:00Z").unwrap()),
            now(),
        )
        .unwrap_err();
        assert!(e.contains("not in the future"), "{e}");
    }

    /// A key file is never overwritten: doing so loses the ability to re-make
    /// every signature it ever made.
    #[test]
    fn a_key_file_is_never_overwritten() {
        let _seeded = testfs::seed(true, &[]);
        let path = std::path::PathBuf::from("/r/k");
        new(&path, &mut Vec::new()).unwrap();
        let e = new(&path, &mut Vec::new()).unwrap_err();
        assert!(e.contains("never overwritten"), "{e}");
    }

    #[test]
    fn a_file_that_is_not_a_key_is_refused_by_name() {
        let _seeded = testfs::seed(true, &[("/r/notes.txt", "this is not a key")]);
        // No `unwrap_err`: `SecretKey` has no `Debug`, which is deliberate and
        // worth keeping -- a private key should not be printable by accident
        // any more than it should be serializable by accident.
        let Err(e) = load(std::path::Path::new("/r/notes.txt")) else {
            panic!("a file of prose is not a key");
        };
        assert!(e.contains("notes.txt"), "it names the file: {e}");
        assert!(
            e.contains("a key file") || e.contains("ed25519 wants 32"),
            "{e}"
        );
    }

    /// Two keys are two keys: the entropy is the platform's, and a key that
    /// repeated would sign for somebody else.
    #[test]
    fn two_generated_keys_differ() {
        let _seeded = testfs::seed(true, &[]);
        let mut first = Vec::new();
        let mut second = Vec::new();
        new(std::path::Path::new("/r/a"), &mut first).unwrap();
        new(std::path::Path::new("/r/b"), &mut second).unwrap();
        assert_ne!(first, second);
    }
}
