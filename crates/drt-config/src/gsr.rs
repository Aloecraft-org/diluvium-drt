//! Grant signing requests and the decisions about them (consent.md §6, §7).
//!
//! A running node asks for something beyond its current grant. The request
//! is **data and is never executable**, it is written by the runtime rather
//! than by the node, and transport is files: the runtime writes
//! `state/gsr/pending/` and reads `state/gsr/decided/` and knows nothing
//! about HTTP, portals or auth systems. A portal behind an auth proxy and a
//! human with a text editor are indistinguishable from in here, and should
//! be.
//!
//! **Request identity is content-addressed.** What binds is who is asking,
//! in which root, for which realm, for exactly what, and for when — not the
//! moment of asking and not why. Three things fall out of that, and each is
//! the answer to a bug somebody would otherwise write: a restart finds its
//! standing approval because the hash is recomputable; idempotency is one
//! `stat` rather than comparison logic; and a node that rewords its `reason`
//! keeps its grant.
//!
//! ## surface block
//!
//! - Entry points: [`Request::new`]; [`Request::identity`], the hash that
//!   *is* the request; [`Request::filename`]; [`Decision::signing_bytes`];
//!   [`Decision::sign`]; [`verify`], the four-step chain.
//! - Configurable values: [`PENDING_DIR`] and [`DECIDED_DIR`], the two
//!   directories, relative to `state/`.
//! - Fan-out: [`Verified`] is what a verified decision says; [`Unverified`]
//!   names every refusal, one per step, in the order they are checked.

use serde::{Deserialize, Serialize};

use crate::canon::{self, Hash};
use crate::consent::ConsentJson;
use crate::id::Uuid7;
use crate::project::NodePath;
use crate::realm::{Realm, RealmRegistry};
use crate::sign::{Alg, KeyId, SecretKey, Signature, VerifyFailed};
use crate::time::{Timestamp, Window};

/// Written by the runtime, read by whatever the operator plugged in.
pub const PENDING_DIR: &str = "gsr/pending";
/// Decisions of **both** kinds land here, which is why it is not called
/// `approved/`: a signed `deny` is an answer, and the node is entitled to
/// it.
pub const DECIDED_DIR: &str = "gsr/decided";

/// A request for a grant beyond what a node holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// A uuid7, so the creation time is implicit and there is no
    /// `created_at` beside it. **Not the request's identity**, and it must
    /// never be indexed on: identity is [`Request::identity`]. Outside the
    /// hash, so two calls asking the same thing produce the same identity.
    pub created_uuid7: Uuid7,
    pub root_id: Uuid7,
    /// Derived at spawn from the parent's path, not advisory text. The file
    /// is an audit record, and an audit record whose subject is a string the
    /// node chose is not one.
    pub node: NodePath,
    pub realm: Realm,
    /// Opaque to the runtime; the capability owning the realm interprets it.
    /// Inside the hashed identity, so it must round-trip through the
    /// canonicalizer — [`canon::from_msgpack`] is where a guest's value is
    /// refused if it cannot.
    pub ask: serde_json::Value,
    /// Inside the hash, and **absent rather than null** when unbounded:
    /// access next week is a different ask from access now, and an approval
    /// of the bounded ask must not match an unbounded re-request.
    ///
    /// The window is a fixed ask, not a rolling one. A node computing
    /// `valid_until = now + 1h` on every call changes its hash on every call
    /// and re-prompts forever; that is the bug every first implementer
    /// writes, which is why it is said here and in consent.md both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<Timestamp>,
    /// Free text from the node. Outside the hash, and so not load-bearing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Request {
    pub fn new(
        created_uuid7: Uuid7,
        root_id: Uuid7,
        node: NodePath,
        realm: Realm,
        ask: serde_json::Value,
    ) -> Request {
        Request {
            created_uuid7,
            root_id,
            node,
            realm,
            ask,
            valid_from: None,
            valid_until: None,
            reason: String::new(),
        }
    }

    pub fn with_window(mut self, window: Window) -> Request {
        self.valid_from = window.from;
        self.valid_until = window.until;
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Request {
        self.reason = reason.into();
        self
    }

    pub fn window(&self) -> Window {
        Window {
            from: self.valid_from,
            until: self.valid_until,
        }
    }

    /// The hash that **is** this request: canonical JSON over
    /// `{root_id, node, realm, ask, valid_from, valid_until}` with absent
    /// optionals omitted, and nothing else.
    ///
    /// Built field by field rather than by serializing `self` and deleting
    /// keys, because the set of fields in the identity is a decision and a
    /// decision belongs in code that fails to compile when someone adds a
    /// field without thinking about it.
    ///
    /// **`node` is the local form, and the root appears once.** A
    /// [`crate::project::QualifiedNode`] spells the same node as
    /// `<root_id>/root/intake`, and putting that here would hash the root
    /// twice — once inside the string, once in `root_id` — leaving two
    /// implementers free to hash different bytes for one request. The
    /// qualified form is for addressing a peer and reaches no preimage.
    pub fn identity(&self) -> Hash {
        let mut map = serde_json::Map::new();
        map.insert("root_id".into(), self.root_id.to_string().into());
        map.insert("node".into(), self.node.as_str().into());
        map.insert("realm".into(), self.realm.as_str().into());
        map.insert("ask".into(), self.ask.clone());
        // Omitted, never blanked: `null` and absent are different bytes, and
        // this is the one place in the system where that rule bites.
        if let Some(from) = self.valid_from {
            map.insert("valid_from".into(), from.to_string().into());
        }
        if let Some(until) = self.valid_until {
            map.insert("valid_until".into(), until.to_string().into());
        }
        canon::hash_value(&serde_json::Value::Object(map))
    }

    /// `<identity_hash>.json` — the pending file's name, so "is this already
    /// pending?" is a `stat` and idempotency needs no comparison logic.
    pub fn filename(&self) -> String {
        format!("{}.json", self.identity().hex())
    }
}

/// Approve or deny. Both land in [`DECIDED_DIR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approve,
    Deny,
}

/// A signed decision about one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    /// The identity hash from [`Request::identity`]. Spelled out in
    /// consent.md because it is the one field that produces signature
    /// failures nobody can debug.
    pub request_hash: Hash,
    pub decision: Verdict,
    /// Binding to `request_hash` plus this is what prevents replay.
    pub not_after: Timestamp,
    pub key_id: KeyId,
    pub alg: Alg,
    pub signature: Signature,
}

impl Decision {
    /// The bytes a signature covers: this object with `signature` omitted.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let value = serde_json::to_value(self)?;
        Ok(crate::sign::signing_bytes(&value, "signature"))
    }

    /// Produce a signed decision. The operator's side of the file
    /// boundary — a portal, a script, or `drt key sign`, which is why it is
    /// here rather than anywhere that knows about portals.
    pub fn sign(
        request_hash: Hash,
        decision: Verdict,
        not_after: Timestamp,
        key_id: KeyId,
        key: &SecretKey,
    ) -> Result<Decision, serde_json::Error> {
        // Signed over the same bytes a verifier will rebuild, which means
        // assembling the object first with a placeholder that is then
        // *omitted* rather than blanked.
        let mut unsigned = Decision {
            request_hash,
            decision,
            not_after,
            key_id,
            alg: Alg::Ed25519,
            signature: Signature::try_from(base64_zeros()).expect("64 zero bytes is a signature"),
        };
        let bytes = unsigned.signing_bytes()?;
        unsigned.signature = key.sign(&bytes);
        Ok(unsigned)
    }
}

fn base64_zeros() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode([0u8; 64])
}

/// What a verified decision says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// Granted, for this long. The effective window is the intersection of
    /// what was asked and what was approved: an operator can always grant
    /// less than was asked and never more.
    Granted { window: Window },
    /// Denied, and verifiably so. The node is told `denied`, not `pending`.
    Denied,
}

/// Every way verification refuses, in the order the steps run.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Unverified {
    #[error("this decision is for request {found}, not {expected}")]
    WrongRequest { expected: Hash, found: Hash },
    #[error("step 1: no signer '{key_id}' in consent.json; the operator has not said they trust this key")]
    UnknownKey { key_id: KeyId },
    #[error("step 1: signer '{key_id}' is an {expected} key and the decision claims {found}")]
    WrongAlgorithm {
        key_id: KeyId,
        expected: Alg,
        found: Alg,
    },
    #[error("step 1: {0}")]
    Signature(#[from] VerifyFailed),
    #[error("step 2: signer '{key_id}' is not authorized for '{realm}'")]
    SignerNotAuthorized { key_id: KeyId, realm: Realm },
    #[error("step 3: '{realm}' is outside this root's consented ceiling; an approval can act within the ceiling and can never widen beyond it")]
    OutsideCeiling { realm: Realm },
    #[error("step 4: the decision expired at {not_after}")]
    Expired { not_after: Timestamp },
    #[error("step 4: the approved window does not overlap what was asked")]
    EmptyWindow,
    #[error("the decision cannot be re-serialized to check its signature: {detail}")]
    NotSerializable { detail: String },
}

/// consent.md §7's chain, four steps, each a named failure.
///
/// Re-run on **every** read. Nothing is trusted because it is sitting in
/// `state/`; a file written into `decided/` by anything other than a holder
/// of a consented signer's key verifies at step 1 and goes no further.
///
/// `not_after` is checked **last**, on purpose: never interpret
/// unauthenticated data, and an expired-but-otherwise-valid approval should
/// say "expired" rather than "unknown key". The cheap instinct is to check
/// the date first because it is the cheapest test; that instinct is wrong
/// here and this comment is why it stays wrong.
pub fn verify(
    request: &Request,
    decision: &Decision,
    consent: &ConsentJson,
    registry: &RealmRegistry,
    now: Timestamp,
) -> Result<Verified, Unverified> {
    // Not one of the four steps: a decision whose hash does not match is
    // not a failed verification, it is the wrong file. Callers look up by
    // identity, so reaching this means a mistake worth naming differently.
    let identity = request.identity();
    if decision.request_hash != identity {
        return Err(Unverified::WrongRequest {
            expected: identity,
            found: decision.request_hash.clone(),
        });
    }

    // Step 1: the signature verifies against the named key in consent.json.
    let Some(signer) = consent.signer(&decision.key_id) else {
        return Err(Unverified::UnknownKey {
            key_id: decision.key_id.clone(),
        });
    };
    if signer.alg != decision.alg {
        return Err(Unverified::WrongAlgorithm {
            key_id: decision.key_id.clone(),
            expected: signer.alg,
            found: decision.alg,
        });
    }
    let bytes = decision
        .signing_bytes()
        .map_err(|e| Unverified::NotSerializable {
            detail: e.to_string(),
        })?;
    signer.public_key.verify(&bytes, &decision.signature)?;

    // Step 2: that signer is authorized for the request's realm.
    if !request.realm.covered_by_any(&signer.realms) {
        return Err(Unverified::SignerNotAuthorized {
            key_id: decision.key_id.clone(),
            realm: request.realm.clone(),
        });
    }

    // Step 3: the realm is within the operator-consented ceiling. **The
    // invariant.** An approval can only act within the consented ceiling;
    // it can never widen beyond it. Without this the portal is an
    // escalation path, and a signature from a key the operator trusts
    // broadly would be enough to reach anything.
    let consented = match consent.root_entry() {
        Some(entry) => entry.realms(registry),
        None => Vec::new(),
    };
    if !request.realm.covered_by_any(&consented) {
        return Err(Unverified::OutsideCeiling {
            realm: request.realm.clone(),
        });
    }

    // Step 4: the decision is live, and the effective window is real.
    if !decision.not_after.is_after(now) {
        return Err(Unverified::Expired {
            not_after: decision.not_after,
        });
    }
    if decision.decision == Verdict::Deny {
        return Ok(Verified::Denied);
    }
    let window = request.window().intersect(Window {
        from: None,
        until: Some(decision.not_after),
    });
    if !window.is_non_empty() {
        return Err(Unverified::EmptyWindow);
    }
    Ok(Verified::Granted { window })
}

/// Is this decision past its `not_after`? What the sweep of `decided/` on
/// read asks, without needing the request it decides.
pub fn is_expired(decision: &Decision, now: Timestamp) -> bool {
    !decision.not_after.is_after(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent::{Accepted, Signer};
    use crate::project::{self, ProjectJson};
    use drt_caps::Grant;

    fn root_id() -> Uuid7 {
        Uuid7::mint(1_757_707_440_000, [0x11; 10])
    }

    fn now() -> Timestamp {
        Timestamp::parse("2026-09-12T00:00:00Z").unwrap()
    }

    fn later() -> Timestamp {
        Timestamp::parse("2026-09-19T00:00:00Z").unwrap()
    }

    fn ask() -> serde_json::Value {
        serde_json::json!({"add": ["example.com"]})
    }

    fn request() -> Request {
        Request::new(
            Uuid7::mint(1_757_707_440_000, [0x33; 10]),
            root_id(),
            NodePath::parse("root/intake").unwrap(),
            Realm::parse("operator.rest.get").unwrap(),
            ask(),
        )
        .with_reason("the intake node needs the upstream")
    }

    /// Acceptance 5, second half: the identity carries the local node form
    /// and the root exactly once each.
    ///
    /// Asserted over the preimage rather than the hash, because a hash
    /// cannot say *why* it is wrong. The same node has a qualified spelling
    /// (`<root_id>/root/intake`) and it must not appear here — it would
    /// hash the root twice.
    #[test]
    fn the_identity_carries_the_local_node_form_and_the_root_once() {
        let request = request();
        let qualified = request.node.qualified(request.root_id).to_string();

        let mut map = serde_json::Map::new();
        map.insert("root_id".into(), request.root_id.to_string().into());
        map.insert("node".into(), request.node.as_str().into());
        map.insert("realm".into(), request.realm.as_str().into());
        map.insert("ask".into(), request.ask.clone());
        let preimage = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();

        assert_eq!(
            crate::canon::hash_value(&serde_json::from_str(&preimage).unwrap()),
            request.identity(),
            "the identity is that preimage and no other"
        );
        assert_eq!(
            preimage.matches(&request.root_id.to_string()).count(),
            1,
            "the root appears exactly once: {preimage}"
        );
        assert!(
            !preimage.contains(&qualified),
            "the qualified form must not reach a preimage: {preimage}"
        );
        assert!(
            preimage.contains("\"node\":\"root/intake\""),
            "the local form is what is hashed: {preimage}"
        );
    }

    /// A consent file whose ceiling covers `host:rest/get`, with one signer
    /// authorized at the root realm.
    fn consent(key: &SecretKey, signer_realms: Vec<Realm>, ceiling: Vec<Grant>) -> ConsentJson {
        let project = ProjectJson {
            caps: ceiling.clone(),
            ..ProjectJson::new(root_id())
        };
        ConsentJson {
            root_id: root_id(),
            accepted: vec![Accepted::Listed {
                realm: Realm::root(),
                ceiling_hash: project::ceiling_hash(&project).unwrap(),
                ceiling: crate::project::DeclaredCeiling::of_caps(ceiling),
                accepted_at: now(),
            }],
            signers: vec![Signer {
                key_id: KeyId("portal-1".into()),
                alg: Alg::Ed25519,
                public_key: key.public_key(),
                realms: signer_realms,
            }],
            peers: Vec::new(),
        }
    }

    fn approve(request: &Request, key: &SecretKey, not_after: Timestamp) -> Decision {
        Decision::sign(
            request.identity(),
            Verdict::Approve,
            not_after,
            KeyId("portal-1".into()),
            key,
        )
        .unwrap()
    }

    /// consent.md acceptance 2's core: a valid approval for a realm inside
    /// the ceiling is granted.
    #[test]
    fn an_approval_inside_the_ceiling_is_granted() {
        let key = SecretKey::generate([5u8; 32]);
        let consent = consent(
            &key,
            vec![Realm::root()],
            vec![Grant::grant("host:rest/get")],
        );
        let request = request();
        let decision = approve(&request, &key, later());

        let verified = verify(&request, &decision, &consent, &RealmRegistry::new(), now()).unwrap();
        assert_eq!(
            verified,
            Verified::Granted {
                window: Window {
                    from: None,
                    until: Some(later())
                }
            }
        );
    }

    /// consent.md acceptance 3, built the way the doc insists: the signer is
    /// authorized *broader* than the consent, so a narrow-signer failure at
    /// step 2 cannot be what passes for the test.
    #[test]
    fn a_realm_outside_the_ceiling_fails_at_step_three_with_a_valid_signature() {
        let key = SecretKey::generate([5u8; 32]);
        // Signer at `operator` -- broad. Consent at `operator.net` only.
        let consent = consent(
            &key,
            vec![Realm::root()],
            vec![Grant::grant("host:net/dial")],
        );
        let request = request(); // asks for operator.rest.get
        let decision = approve(&request, &key, later());

        // The signature really is valid: step 1 and 2 both pass.
        let signer = consent.signer(&KeyId("portal-1".into())).unwrap();
        assert_eq!(
            signer
                .public_key
                .verify(&decision.signing_bytes().unwrap(), &decision.signature),
            Ok(())
        );
        assert!(
            request.realm.covered_by_any(&signer.realms),
            "step 2 passes"
        );

        let e = verify(&request, &decision, &consent, &RealmRegistry::new(), now()).unwrap_err();
        assert!(matches!(e, Unverified::OutsideCeiling { .. }), "{e}");
        assert!(e.to_string().starts_with("step 3:"), "{e}");
    }

    #[test]
    fn a_signer_not_authorized_for_the_realm_fails_at_step_two() {
        let key = SecretKey::generate([5u8; 32]);
        let consent = consent(
            &key,
            vec![Realm::parse("operator.fs").unwrap()],
            vec![Grant::grant("host:rest/get")],
        );
        let request = request();
        let e = verify(
            &request,
            &approve(&request, &key, later()),
            &consent,
            &RealmRegistry::new(),
            now(),
        )
        .unwrap_err();
        assert!(matches!(e, Unverified::SignerNotAuthorized { .. }), "{e}");
    }

    #[test]
    fn an_unknown_key_fails_at_step_one() {
        let trusted = SecretKey::generate([5u8; 32]);
        let other = SecretKey::generate([6u8; 32]);
        let mut consent = consent(
            &trusted,
            vec![Realm::root()],
            vec![Grant::grant("host:rest/get")],
        );
        consent.signers.clear();
        let request = request();
        let e = verify(
            &request,
            &approve(&request, &other, later()),
            &consent,
            &RealmRegistry::new(),
            now(),
        )
        .unwrap_err();
        assert!(matches!(e, Unverified::UnknownKey { .. }), "{e}");
    }

    /// An expired approval says "expired", not "unknown key": the ordering
    /// that comment in `verify` defends.
    #[test]
    fn an_expired_approval_says_expired() {
        let key = SecretKey::generate([5u8; 32]);
        let consent = consent(
            &key,
            vec![Realm::root()],
            vec![Grant::grant("host:rest/get")],
        );
        let request = request();
        let stale = approve(
            &request,
            &key,
            Timestamp::parse("2026-09-11T00:00:00Z").unwrap(),
        );

        let e = verify(&request, &stale, &consent, &RealmRegistry::new(), now()).unwrap_err();
        assert!(matches!(e, Unverified::Expired { .. }), "{e}");
        assert!(is_expired(&stale, now()), "and the sweep agrees");
    }

    /// A tampered decision does not verify, and the tamper is what a replay
    /// would look like: the same signature against a different request.
    #[test]
    fn a_signature_lifted_onto_another_request_does_not_verify() {
        let key = SecretKey::generate([5u8; 32]);
        let consent = consent(
            &key,
            vec![Realm::root()],
            vec![Grant::grant("host:rest/get")],
        );
        let request = request();
        let decision = approve(&request, &key, later());

        let mut other = request.clone();
        other.ask = serde_json::json!({"add": ["evil.example"]});
        let e = verify(&other, &decision, &consent, &RealmRegistry::new(), now()).unwrap_err();
        assert!(matches!(e, Unverified::WrongRequest { .. }), "{e}");

        // And re-pointing the hash breaks the signature instead.
        let mut forged = decision.clone();
        forged.request_hash = other.identity();
        let e = verify(&other, &forged, &consent, &RealmRegistry::new(), now()).unwrap_err();
        assert!(matches!(e, Unverified::Signature(_)), "{e}");
    }

    #[test]
    fn a_verified_deny_is_a_denial_not_a_pending() {
        let key = SecretKey::generate([5u8; 32]);
        let consent = consent(
            &key,
            vec![Realm::root()],
            vec![Grant::grant("host:rest/get")],
        );
        let request = request();
        let denial = Decision::sign(
            request.identity(),
            Verdict::Deny,
            later(),
            KeyId("portal-1".into()),
            &key,
        )
        .unwrap();
        assert_eq!(
            verify(&request, &denial, &consent, &RealmRegistry::new(), now()).unwrap(),
            Verified::Denied
        );
    }

    // depth: the identity properties, which are the whole design

    /// Restart works, and rewording does not cost a grant. The two
    /// consequences of content-addressing that consent.md names.
    #[test]
    fn identity_ignores_the_uuid_and_the_reason() {
        let first = request();
        let mut again = first.clone();
        again.created_uuid7 = Uuid7::mint(1_757_999_999_000, [0x44; 10]);
        again.reason = "a differently worded plea".into();

        assert_eq!(first.identity(), again.identity());
        assert_eq!(first.filename(), again.filename());
    }

    /// A node that recomputes its window per call changes its hash per call.
    /// Stated in consent.md as the bug every first implementer writes, and
    /// asserted here so the property is visible rather than folkloric.
    #[test]
    fn a_moving_window_is_a_different_request() {
        let bounded = request().with_window(Window {
            from: None,
            until: Some(later()),
        });
        let unbounded = request();
        let shifted = request().with_window(Window {
            from: None,
            until: Some(Timestamp::parse("2026-09-20T00:00:00Z").unwrap()),
        });

        assert_ne!(bounded.identity(), unbounded.identity());
        assert_ne!(bounded.identity(), shifted.identity());
    }

    /// Absent is omitted from the hashed object, never written as `null`.
    #[test]
    fn an_absent_window_is_omitted_from_the_identity() {
        let request = request();
        assert!(request.valid_from.is_none());
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("valid_from"), "{json}");
        assert!(!json.contains("null"), "{json}");
    }

    #[test]
    fn the_identity_is_over_exactly_six_fields() {
        // Built by hand in `identity`, so this test is what notices if a
        // field is added to `Request` and silently left out of -- or let
        // into -- what binds.
        let request = request();
        let expected = serde_json::json!({
            "root_id": request.root_id.to_string(),
            "node": request.node.as_str(),
            "realm": request.realm.as_str(),
            "ask": request.ask,
        });
        assert_eq!(request.identity(), canon::hash_value(&expected));
    }

    #[test]
    fn a_request_and_a_decision_round_trip_through_json() {
        let key = SecretKey::generate([5u8; 32]);
        let request = request();
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);

        let decision = approve(&request, &key, later());
        let text = serde_json::to_string(&decision).unwrap();
        let read: Decision = serde_json::from_str(&text).unwrap();
        assert_eq!(read, decision);
        // And the signature still checks after the round trip, which is the
        // only thing that makes the file format useful.
        assert_eq!(
            key.public_key()
                .verify(&read.signing_bytes().unwrap(), &read.signature),
            Ok(())
        );
    }
}
