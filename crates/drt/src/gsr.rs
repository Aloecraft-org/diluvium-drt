//! The grants desk: where a node's `request_grant` lands, and where a signed
//! decision about it is read back (consent.md §6, §7).
//!
//! Files only. This file writes `state/gsr/pending/` and reads
//! `state/gsr/decided/`, and knows nothing about HTTP, portals or auth
//! systems. A portal behind an auth proxy and a human with a text editor are
//! indistinguishable from in here, and should be.
//!
//! ## surface block
//!
//! - Entry points: [`Desk`], which is the `GrantDesk` the dispatcher is given;
//!   [`Desk::new`].
//! - Configurable values: none. The directory names are
//!   `drt_config::gsr`'s, and the realm mapping is the registry's.
//! - Fan-out: [`Desk::request`]'s three returns (`pending`, `granted`,
//!   `denied`) are the whole of what a node can be told, and the lookup order
//!   above them is the one thing a reader must not reorder.
//!
//! **The lookup order is load-bearing and is stated rather than implied.**
//! Compute the identity hash, check `decided/` for a matching *verified*
//! decision, then check `pending/`, then and only then write. Reading
//! `decided/` "on the next call" does not by itself fix the order, and getting
//! it wrong means a restart writes a second pending file for a request that
//! was already answered — which is exactly the bug content-addressing exists
//! to prevent.
//!
//! **Nothing is trusted because it is in `state/`.** The four-step chain runs
//! on every read, and `project.json` and `consent.json` are re-read per
//! request rather than cached: a ceiling the operator has since narrowed must
//! bind, and a signer they have since added should not need a restart. A
//! grant request is a rare call, so the read costs nothing worth a cache and
//! a cache here would be a stale-ceiling bug waiting to be written.

use std::path::PathBuf;

use drt_config::canon;
use drt_config::consent::ConsentJson;
use drt_config::gsr::{self, Decision, Request, Unverified, Verified};
use drt_config::project::{NodePath, ProjectJson};
use drt_config::realm::{Realm, RealmRegistry};
use drt_config::time::{Timestamp, Window};

use crate::drt_root::{self, Root};

/// The desk for one root.
pub struct Desk {
    root: Root,
    registry: RealmRegistry,
}

impl Desk {
    pub fn new(root: Root) -> Desk {
        Desk {
            root,
            // The taxonomy is not this session's to invent (consent.md §3), so
            // the structural default stands until capability families declare
            // their own. Declaring one is a call on this registry and nothing
            // else changes.
            registry: RealmRegistry::new(),
        }
    }

    /// This root's descriptor and the operator's consent, read now.
    fn files(&self) -> Result<(ProjectJson, ConsentJson), String> {
        let project: ProjectJson = read(&self.root.project_json())?;
        let consent: ConsentJson = read(&self.root.consent_json())?;
        if consent.root_id != project.root_id {
            return Err(format!(
                "consent.json is for root {}, but this root is {}",
                consent.root_id, project.root_id
            ));
        }
        Ok((project, consent))
    }
}

impl drt_connector::GrantDesk for Desk {
    fn ceiling(&self) -> Vec<String> {
        match read::<ProjectJson>(&self.root.project_json()) {
            Ok(project) => project
                .caps
                .iter()
                .filter(|g| g.effect == drt_caps::Effect::Grant)
                .map(|g| g.capability.clone())
                .collect(),
            // A root whose descriptor will not read has no ceiling to report.
            // Empty rather than wide: `within_ceiling` is a promise about what
            // could be allowed, and guessing generously is the one direction
            // that would mislead a node into asking for the impossible.
            Err(_) => Vec::new(),
        }
    }

    fn request(&self, node: &str, args: Option<&rmpv::Value>) -> Result<rmpv::Value, String> {
        let (project, consent) = self.files()?;
        let node = NodePath::parse(node).map_err(|e| e.to_string())?;
        let ask = Asked::decode(args)?;

        let request = Request::new(
            drt_root::mint()?,
            project.root_id,
            node,
            ask.realm,
            ask.value,
        )
        .with_window(ask.window)
        .with_reason(ask.reason);

        let identity = request.identity();
        let now = drt_root::now();
        self.root.ensure_state()?;

        // 1. `decided/` first. A restart recomputes the same identity, so a
        //    standing decision is found before anything is written.
        if let Some(answer) = self.decided(&request, &consent, now)? {
            return Ok(answer);
        }

        // 2. `pending/` second. Structural idempotency: the file is named by
        //    the identity, so this is one `stat` and no comparison logic.
        let pending = self.root.gsr_pending().join(request.filename());
        if drt_platform::fs::exists(&pending) {
            return Ok(pending_value(&identity));
        }

        // 3. Only now, write.
        let text = serde_json::to_string_pretty(&request)
            .map_err(|e| format!("cannot serialize the request: {e}"))?;
        drt_platform::fs::write(&pending, format!("{text}\n"))
            .map_err(|e| format!("cannot write {}: {e}", pending.display()))?;
        Ok(pending_value(&identity))
    }
}

// depth: reading decided/, verifying, and the sweep

impl Desk {
    /// A verified decision about this request, if one is there.
    ///
    /// Every file in `decided/` is read and verified rather than the one named
    /// by the identity: a decision is named by whatever produced it, and
    /// requiring a filename convention from "a human with a text editor" would
    /// be a transport detail leaking into §8's promise. The directory is small
    /// by construction -- one file per outstanding ask -- and expired entries
    /// are swept here, which is the only sweep there is.
    fn decided(
        &self,
        request: &Request,
        consent: &ConsentJson,
        now: Timestamp,
    ) -> Result<Option<rmpv::Value>, String> {
        let dir = self.root.gsr_decided();
        let names = drt_platform::fs::read_dir(&dir).unwrap_or_default();
        let identity = request.identity();

        for name in names {
            let path = dir.join(&name);
            let Ok(decision) = read::<Decision>(&path) else {
                // A file that is not a decision is left alone. `decided/` is
                // somebody else's directory to write, and deleting what we
                // cannot parse would be this process editing their side of
                // §8's boundary.
                continue;
            };
            if gsr::is_expired(&decision, now) {
                let _ = drt_platform::fs::remove_file(&path);
                continue;
            }
            if decision.request_hash != identity {
                continue;
            }
            return match gsr::verify(request, &decision, consent, &self.registry, now) {
                Ok(Verified::Granted { window }) => {
                    // Consumed: the pending file goes, and the decision stays
                    // until `not_after`. The request is recomputable from the
                    // node's next call, so deleting the pending file destroys
                    // no preimage -- which is the property that made the
                    // identity content-addressed in the first place.
                    let _ = drt_platform::fs::remove_file(
                        self.root.gsr_pending().join(request.filename()),
                    );
                    Ok(Some(granted_value(window)))
                }
                Ok(Verified::Denied) => {
                    let _ = drt_platform::fs::remove_file(
                        self.root.gsr_pending().join(request.filename()),
                    );
                    Ok(Some(denied_value("the operator denied this request")))
                }
                // A decision that does not verify is not an answer. It is
                // reported to the node as a denial naming the step that failed,
                // and the file is left where it is: deleting evidence of a
                // failed verification is the last thing this should do.
                Err(e) => Ok(Some(denied_value(&step_of(&e)))),
            };
        }
        Ok(None)
    }
}

/// A verification failure, as a node should hear it.
///
/// The step number survives into the message because consent.md makes each
/// step a named failure and because the four mean different things to whoever
/// has to fix it: step 1 is a key the operator has not trusted, step 2 a
/// signer reaching beyond its realms, step 3 an approval trying to widen past
/// the ceiling -- the one the invariant exists for -- and step 4 an expiry.
fn step_of(e: &Unverified) -> String {
    e.to_string()
}

fn pending_value(identity: &canon::Hash) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("status".into(), "pending".into()),
        ("request".into(), identity.as_str().into()),
    ])
}

fn granted_value(window: Window) -> rmpv::Value {
    let mut map = vec![("status".into(), "granted".into())];
    if let Some(from) = window.from {
        map.push(("from".into(), from.to_string().into()));
    }
    if let Some(until) = window.until {
        map.push(("until".into(), until.to_string().into()));
    }
    rmpv::Value::Map(map)
}

fn denied_value(reason: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("status".into(), "denied".into()),
        ("reason".into(), reason.into()),
    ])
}

// depth: decoding the call's arguments

/// What a node asked for, decoded and validated.
struct Asked {
    realm: Realm,
    value: serde_json::Value,
    reason: String,
    window: Window,
}

impl Asked {
    fn decode(args: Option<&rmpv::Value>) -> Result<Asked, String> {
        let args = args.ok_or("request_grant takes {realm, ask, reason} and got nothing")?;
        let field = |name: &str| -> Option<&rmpv::Value> {
            args.as_map()?
                .iter()
                .find(|(k, _)| k.as_str() == Some(name))
                .map(|(_, v)| v)
        };
        let realm = field("realm")
            .and_then(|v| v.as_str())
            .ok_or("request_grant: `realm` is required and is a dotted path")?;
        let realm = Realm::parse(realm).map_err(|e| e.to_string())?;

        // The `ask` crosses from a guest as msgpack, and this is the last
        // place a non-finite float or a duplicate key is still visible --
        // consent.md §9's explicit walk. An ask that cannot be canonicalized
        // cannot be hashed, so it cannot be signed, so it is refused here
        // rather than at a verification nobody can debug.
        let value = match field("ask") {
            Some(ask) => canon::from_msgpack(ask).map_err(|e| e.to_string())?,
            None => serde_json::Value::Null,
        };

        let instant = |name: &str| -> Result<Option<Timestamp>, String> {
            match field(name).and_then(|v| v.as_str()) {
                Some(text) => Timestamp::parse(text).map(Some).map_err(|e| e.to_string()),
                None => Ok(None),
            }
        };
        Ok(Asked {
            realm,
            value,
            reason: field("reason")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            window: Window {
                from: instant("valid_from")?,
                until: instant("valid_until")?,
            },
        })
    }
}

fn read<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<T, String> {
    let text = drt_platform::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drt_caps::{CapSet, Grant, Principal};
    use drt_config::consent::{Accepted, Signer};
    use drt_config::gsr::Verdict;
    use drt_config::sign::{Alg, KeyId, SecretKey};
    use drt_connector::GrantDesk as _;

    use crate::testfs::{self, Seeded};

    fn key() -> SecretKey {
        SecretKey::generate([5u8; 32])
    }

    /// A root whose ceiling allows `host:rest/get` -- so `operator.rest.get`
    /// is inside it -- with one signer trusted at the root realm.
    fn consented(seeded: &Seeded, signer_realms: &[&str], ceiling: &[&str]) {
        let caps: Vec<Grant> = ceiling.iter().map(|c| Grant::grant(*c)).collect();
        let project = ProjectJson {
            caps: caps.clone(),
            ..ProjectJson::new(Seeded::root_id())
        };
        let consent = ConsentJson {
            root_id: Seeded::root_id(),
            accepted: vec![Accepted::Listed {
                realm: Realm::root(),
                ceiling_hash: drt_config::project::ceiling_hash(&project).unwrap(),
                ceiling: drt_config::project::DeclaredCeiling::of_caps(caps),
                accepted_at: drt_root::now(),
            }],
            signers: vec![Signer {
                key_id: KeyId("portal-1".into()),
                alg: Alg::Ed25519,
                public_key: key().public_key(),
                realms: signer_realms
                    .iter()
                    .map(|r| Realm::parse(r).unwrap())
                    .collect(),
            }],
        };
        drt_platform::fs::write(
            seeded.root.project_json(),
            serde_json::to_string(&project).unwrap(),
        )
        .unwrap();
        seeded.root.write_consent(&consent).unwrap();
    }

    fn ask(realm: &str) -> rmpv::Value {
        rmpv::Value::Map(vec![
            ("realm".into(), realm.into()),
            (
                "ask".into(),
                rmpv::Value::Map(vec![(
                    "add".into(),
                    rmpv::Value::Array(vec!["example.com".into()]),
                )]),
            ),
            ("reason".into(), "the intake node needs the upstream".into()),
        ])
    }

    fn status(value: &rmpv::Value) -> &str {
        value
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("status")))
            .and_then(|(_, v)| v.as_str())
            .unwrap_or("?")
    }

    fn reason(value: &rmpv::Value) -> String {
        value
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("reason")))
            .and_then(|(_, v)| v.as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// The request identity as the desk computes it, so a test can sign the
    /// same bytes the verifier will rebuild.
    fn identity(seeded: &Seeded, realm: &str) -> canon::Hash {
        let names = drt_platform::fs::read_dir(seeded.root.gsr_pending()).unwrap();
        assert_eq!(names.len(), 1, "exactly one pending file: {names:?}");
        let request: Request = read(&seeded.root.gsr_pending().join(&names[0])).expect("it parses");
        assert_eq!(request.realm.as_str(), realm);
        request.identity()
    }

    fn approve(hash: canon::Hash, verdict: Verdict, not_after: &str) -> Decision {
        Decision::sign(
            hash,
            verdict,
            Timestamp::parse(not_after).unwrap(),
            KeyId("portal-1".into()),
            &key(),
        )
        .unwrap()
    }

    fn land(seeded: &Seeded, decision: &Decision) {
        drt_platform::fs::write(
            seeded.root.gsr_decided().join("from-the-portal.json"),
            serde_json::to_string_pretty(decision).unwrap(),
        )
        .unwrap();
    }

    /// consent.md acceptance 2, end to end, restart included.
    #[test]
    fn an_ask_pends_then_is_granted_and_survives_a_restart() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        // First call: pending, and one file written.
        let first = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&first), "pending");

        // Same ask again: still pending, and still one file. Structural
        // idempotency -- the obvious node implementation is a loop.
        let again = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&again), "pending");
        assert_eq!(
            drt_platform::fs::read_dir(seeded.root.gsr_pending())
                .unwrap()
                .len(),
            1,
            "a loop does not fill the directory"
        );

        // A valid signed approval lands.
        let hash = identity(&seeded, "operator.rest.get");
        land(
            &seeded,
            &approve(hash, Verdict::Approve, "2099-01-01T00:00:00Z"),
        );

        let granted = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&granted), "granted", "{granted:?}");
        assert!(
            drt_platform::fs::read_dir(seeded.root.gsr_pending())
                .unwrap()
                .is_empty(),
            "the pending file is gone once consumed"
        );

        // Restart: a fresh desk over the same root. The node asks the same
        // thing, the identity recomputes to the same hash, and the standing
        // decision is found -- with no new pending file, which is the half
        // that an identity carrying the moment of asking would have lost.
        let restarted = Desk::new(seeded.root.clone());
        let after = restarted
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&after), "granted");
        assert!(
            drt_platform::fs::read_dir(seeded.root.gsr_pending())
                .unwrap()
                .is_empty(),
            "and it wrote nothing"
        );
    }

    /// consent.md acceptance 3, constructed so the signer is authorized
    /// *broader* than the consent: a narrowly scoped signer would fail at step
    /// 2 and prove nothing about the invariant.
    #[test]
    fn a_realm_outside_the_ceiling_fails_at_step_three() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:fs/read"]);
        let desk = Desk::new(seeded.root.clone());

        desk.request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        let hash = identity(&seeded, "operator.rest.get");
        land(
            &seeded,
            &approve(hash, Verdict::Approve, "2099-01-01T00:00:00Z"),
        );

        let answer = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&answer), "denied");
        assert!(
            reason(&answer).starts_with("step 3:"),
            "{}",
            reason(&answer)
        );
        assert!(
            reason(&answer).contains("never widen"),
            "the invariant is what it says: {}",
            reason(&answer)
        );
    }

    #[test]
    fn a_signer_outside_its_realms_fails_at_step_two() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator.fs"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        desk.request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        let hash = identity(&seeded, "operator.rest.get");
        land(
            &seeded,
            &approve(hash, Verdict::Approve, "2099-01-01T00:00:00Z"),
        );

        let answer = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert!(
            reason(&answer).starts_with("step 2:"),
            "{}",
            reason(&answer)
        );
    }

    /// A signed `deny` is an answer, which is why the directory is not called
    /// `approved/`.
    #[test]
    fn a_signed_deny_is_an_answer() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        desk.request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        let hash = identity(&seeded, "operator.rest.get");
        land(
            &seeded,
            &approve(hash, Verdict::Deny, "2099-01-01T00:00:00Z"),
        );

        let answer = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&answer), "denied");
        assert!(
            reason(&answer).contains("operator denied"),
            "{}",
            reason(&answer)
        );
    }

    /// An expired decision is swept on read, and the ask is pending again
    /// rather than silently granted or silently stuck.
    #[test]
    fn an_expired_decision_is_swept_and_the_ask_pends_again() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        desk.request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        let hash = identity(&seeded, "operator.rest.get");
        land(
            &seeded,
            &approve(hash, Verdict::Approve, "2020-01-01T00:00:00Z"),
        );

        let answer = desk
            .request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        assert_eq!(status(&answer), "pending", "{answer:?}");
        assert!(
            drt_platform::fs::read_dir(seeded.root.gsr_decided())
                .unwrap()
                .is_empty(),
            "the stale decision was swept"
        );
    }

    /// A node that recomputes its window per call changes its identity per
    /// call. consent.md names this as the bug every first implementer writes,
    /// so the desk is asserted to behave that way rather than to paper over it.
    #[test]
    fn a_different_window_is_a_different_request() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        let mut bounded = ask("operator.rest.get").as_map().unwrap().to_vec();
        bounded.push(("valid_until".into(), "2026-09-19T00:00:00Z".into()));

        desk.request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        desk.request("root/intake", Some(&rmpv::Value::Map(bounded)))
            .unwrap();
        assert_eq!(
            drt_platform::fs::read_dir(seeded.root.gsr_pending())
                .unwrap()
                .len(),
            2,
            "access for a week is a different ask from access forever"
        );
    }

    /// Rewording a plea does not cost a grant: `reason` rides outside the
    /// identity.
    #[test]
    fn rewording_the_reason_keeps_the_request() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        desk.request("root/intake", Some(&ask("operator.rest.get")))
            .unwrap();
        let mut reworded = ask("operator.rest.get").as_map().unwrap().to_vec();
        reworded.retain(|(k, _)| k.as_str() != Some("reason"));
        reworded.push(("reason".into(), "please, it is urgent".into()));
        desk.request("root/intake", Some(&rmpv::Value::Map(reworded)))
            .unwrap();

        assert_eq!(
            drt_platform::fs::read_dir(seeded.root.gsr_pending())
                .unwrap()
                .len(),
            1,
            "one request, two pleas"
        );
    }

    /// An `ask` that cannot be canonicalized is refused where it is still
    /// visible as one: a NaN becomes `null` the moment it reaches JSON.
    #[test]
    fn an_ask_that_cannot_be_hashed_is_refused_by_name() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get"]);
        let desk = Desk::new(seeded.root.clone());

        let bad = rmpv::Value::Map(vec![
            ("realm".into(), "operator.rest.get".into()),
            (
                "ask".into(),
                rmpv::Value::Map(vec![("rate".into(), rmpv::Value::F64(f64::NAN))]),
            ),
        ]);
        let e = desk.request("root/intake", Some(&bad)).unwrap_err();
        assert!(e.contains("finite"), "{e}");
        // Not merely empty: the refusal happens before `ensure_state`, so a
        // request that cannot exist leaves no directories behind either.
        assert!(
            !drt_platform::fs::exists(seeded.root.state()),
            "an unhashable ask creates no state at all"
        );
    }

    /// The desk reports the ceiling for `capabilities/list`, which is what
    /// lets a node tell "ask" from "reconfigure".
    #[test]
    fn the_desk_reports_the_ceiling() {
        let seeded = testfs::seed(true, &[]);
        consented(&seeded, &["operator"], &["host:rest/get", "host:fs/read"]);
        let desk = Desk::new(seeded.root.clone());
        let mut ceiling = desk.ceiling();
        ceiling.sort();
        assert_eq!(ceiling, ["host:fs/read", "host:rest/get"]);
    }

    /// A dispatcher with no desk answers honestly rather than pretending, and
    /// a set with no node path is named rather than unwrapped.
    #[test]
    fn a_process_with_no_root_says_so() {
        let registry = drt_connector::Registry::new();
        let dispatcher = drt_connector::Dispatcher::new(registry);
        let caps = CapSet::root_held_by(
            Principal("root".into()),
            vec![Grant::grant("host:capabilities/*")],
        );
        let request = drt_hostcall::Request {
            tok: 1,
            call: drt_connector::REQUEST_GRANT.into(),
            args: Some(ask("operator.rest.get")),
        };
        let raw = drt_hostcall::to_bytes(&request).unwrap();
        let reply = pollster::block_on(dispatcher.dispatch(&caps, &raw));
        assert_eq!(reply.status, drt_hostcall::Status::Denied);
        assert!(
            reply
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("deployment"),
            "{reply:?}"
        );
    }
}
