//! `consent.json`: the operator acknowledging a ceiling (consent.md §2).
//!
//! Consent is not a grant and never creates one. The order is: consent
//! bounds the ceiling, the ceiling bounds the root node, attenuation bounds
//! everything below. This file holds the first link only.
//!
//! Operator-owned means **never travels**, not "only dollup writes it": drt
//! writes this file on a first acceptance and on a silent narrowing. It
//! lives beside `project.json` and deliberately *not* in `state/` — the
//! operator-owned versus runtime-owned line is the whole reason `state/`
//! exists.
//!
//! ## surface block
//!
//! - Entry points: [`ConsentJson`], the file as a type; [`check`], the one
//!   function `start`, `dollup consent` and `dollup audit` all call;
//!   [`widen_check_ceiling`], the relation that decides silent-or-prompt,
//!   with [`widen_check`] its caps half; [`Change`], the same edit
//!   described for a human to read; [`PeerBinding`], a role bound to the
//!   peer that satisfies it here.
//! - Configurable: nothing. The modes are [`Accepted`]'s variants.
//! - Fan-out: [`ConsentCheck`] is every answer `check` can give,
//!   [`ConsentFailure`] every way it refuses, and [`PeerKind`] the two
//!   things a peer can be. `start` acts on one arm each; audit renders
//!   them.

use serde::{Deserialize, Serialize};

use drt_caps::{CapSet, Effect, Grant, Principal};

use crate::canon::Hash;
use crate::id::Uuid7;
use crate::project::{self, DeclaredCeiling, ProjectJson};
use crate::realm::{Realm, RealmRegistry};
use crate::sign::{Alg, KeyId, PublicKey};
use crate::time::Timestamp;

/// `.drt_root/consent.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentJson {
    /// Must match `project.json`. A mismatch is a named failure at start:
    /// it is how a `consent.json` that travelled with a copied root, or one
    /// left behind by a different root at this path, is caught.
    pub root_id: Uuid7,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted: Vec<Accepted>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signers: Vec<Signer>,
    /// **Reserved, and refused.** The operator's binding of each role a
    /// `project.json` declares to an actual peer on this box.
    ///
    /// The format is settled — that is what this field is for — and nothing
    /// implements it: a non-empty list is a named failure at start, so a
    /// binding written today is refused rather than half-honoured. It
    /// parses so that the refusal can name the role and the kind, and so
    /// that whoever writes the first one is writing the final shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerBinding>,
}

/// One role, bound to the peer that satisfies it on this box.
///
/// Operator-owned and **never travels**, which is the whole point of the
/// split: a root ships the roles it expects in `project.json`, and the same
/// root runs against a different database on two boxes without editing the
/// root.
///
/// `kind` is the seam between the two things a peer can be. A root is
/// reached by its id on this box; a plugin has no id and is reached by the
/// endpoint its platform provides. Both carry an identity key, because the
/// runtime verifies a peer the same way either way, and both carry the
/// queues the binding covers.
///
/// No `deny_unknown_fields` here, and it is serde's limitation rather than
/// a decision — the same one [`Accepted`] carries. A `flatten`ed field
/// collects what it does not recognise, so the two attributes together
/// refuse `kind` itself. An unknown *kind* is still refused by name, which
/// is the case that matters; an unknown field beside it is ignored. The
/// fix, if it ever needs one, is a hand-written `Deserialize` and not a
/// different shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerBinding {
    /// The role, matching a [`crate::project::PeerDeclaration::role`].
    pub peer: String,
    #[serde(flatten)]
    pub kind: PeerKind,
    /// The queues this binding covers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues: Vec<String>,
}

/// What satisfies a role: another drt root on this box, or a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PeerKind {
    /// Another drt root, addressed by its id. Reciprocity — that root's own
    /// `consent.json` naming this one back — is checkable only on this box,
    /// because a consent file never travels.
    Root {
        root_id: Uuid7,
        public_key: PublicKey,
    },
    /// A plugin, which has no `root_id`. Its key comes from the signed
    /// identity in its package, so the operator confirms a binding rather
    /// than transcribing one. The binding is one way: a plugin names
    /// nobody back.
    Plugin { public_key: PublicKey },
}

impl PeerBinding {
    /// The label a refusal uses: the role and what it is bound to.
    pub fn describe(&self) -> String {
        match &self.kind {
            PeerKind::Root { root_id, .. } => format!("'{}' (root {root_id})", self.peer),
            PeerKind::Plugin { .. } => format!("'{}' (a plugin)", self.peer),
        }
    }
}

/// One acceptance.
///
/// Two variants with **different shapes**, not one struct with an
/// `Option`: `all` has no consulted hash and `listed` has no meaning
/// without one, so the type system holds the "never consulted" line rather
/// than a comment that can be skipped.
///
/// Note on strictness: `deny_unknown_fields` is on [`ConsentJson`] and
/// [`Signer`] but cannot be applied to an internally tagged enum's
/// variants, which is a serde limitation rather than a decision. An unknown
/// `mode` is still refused by name, which is the case that matters; an
/// unknown *field* inside an entry is ignored. If that ever needs closing
/// the fix is a hand-written `Deserialize`, not a different shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Accepted {
    /// Blanket: covers everything under `realm`, forever, and nothing
    /// prompts. It bypasses the whole of consent.md §4 — the delta,
    /// `--accept-changes`, the widening protection. That is what `--all`
    /// *is*, and it is why audit reports it explicitly rather than as one
    /// line of a summary.
    All {
        realm: Realm,
        /// Recorded for audit and **never consulted**: the ceiling as it
        /// stood when the operator took the blanket entry, so audit can say
        /// "accepted against a ceiling that has since changed."
        accepted_against: Hash,
        accepted_at: Timestamp,
    },
    /// Covers what its `ceiling` maps to, and carries a consulted hash.
    Listed {
        realm: Realm,
        ceiling_hash: Hash,
        /// The accepted ceiling, stored **verbatim** beside its hash: both
        /// halves, `caps` and `peers`.
        ///
        /// Without this an entry can only say "changed", never which
        /// direction, and §4's delta cannot be computed at all — a hash is
        /// not invertible. Small, operator-owned, travels nowhere.
        ///
        /// An entry written before `peers` existed holds a bare array here;
        /// [`DeclaredCeiling`] reads that spelling and writes the object, so
        /// an upgrade is a silent re-write rather than a refusal.
        ceiling: DeclaredCeiling,
        accepted_at: Timestamp,
    },
}

impl Accepted {
    pub fn realm(&self) -> &Realm {
        match self {
            Accepted::All { realm, .. } | Accepted::Listed { realm, .. } => realm,
        }
    }

    pub fn accepted_at(&self) -> Timestamp {
        match self {
            Accepted::All { accepted_at, .. } | Accepted::Listed { accepted_at, .. } => {
                *accepted_at
            }
        }
    }

    /// The realms this entry covers.
    ///
    /// Derived, never stored. A stored list could drift from the ceiling it
    /// is supposed to describe; a derived one cannot, and drift here would
    /// mean consent covering something the operator never read.
    pub fn realms(&self, registry: &RealmRegistry) -> Vec<Realm> {
        match self {
            Accepted::All { realm, .. } => vec![realm.clone()],
            Accepted::Listed { ceiling, .. } => registry.realms_of(&ceiling.caps),
        }
    }
}

/// A key the operator trusts to approve grant requests, and for what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signer {
    pub key_id: KeyId,
    pub alg: Alg,
    pub public_key: PublicKey,
    /// The realms this signer may approve within. Plural, and the list is
    /// plural too: consent.md §11 keeps key rotation as a named seam, and a
    /// list that could only hold one key would not be one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub realms: Vec<Realm>,
}

impl ConsentJson {
    /// A fresh file for a root, with no acceptance in it yet.
    pub fn new(root_id: Uuid7) -> ConsentJson {
        ConsentJson {
            root_id,
            accepted: Vec::new(),
            signers: Vec::new(),
            peers: Vec::new(),
        }
    }

    /// The entry that governs this root's ceiling: the one at the root
    /// realm.
    ///
    /// Narrower entries are allowed by the format and govern nothing here —
    /// consenting to a ceiling is a root-level act, so an entry at
    /// `operator.net` says something about a realm, not about whether this
    /// root may start. That is a seam, named and not built.
    pub fn root_entry(&self) -> Option<&Accepted> {
        self.accepted.iter().find(|e| e.realm() == &Realm::root())
    }

    /// The signer named, if the operator trusts it at all. §7 step 1's
    /// lookup.
    pub fn signer(&self, key_id: &KeyId) -> Option<&Signer> {
        self.signers.iter().find(|s| &s.key_id == key_id)
    }
}

/// What `start` must do about consent, and what audit reports.
#[derive(Debug, Clone, PartialEq)]
pub enum ConsentCheck {
    /// Blanket consent is in force. Nothing prompts, now or ever, and §4
    /// does not fire. Audit names this explicitly.
    Blanket {
        accepted_at: Timestamp,
        /// Whether the ceiling has moved since the blanket entry was taken.
        /// Changes nothing; audit says it out loud.
        ceiling_changed: bool,
    },
    /// No acceptance yet: print the ceiling, accept interactively or with
    /// `-y`, write the entry.
    First { ceiling_hash: Hash },
    /// The hash matches. Silent.
    Unchanged,
    /// The ceiling moved and the new one attenuates under the accepted one:
    /// removals only. Silent, and the entry is updated.
    Narrowed { ceiling_hash: Hash, change: Change },
    /// The ceiling moved and it does **not** attenuate: print the delta and
    /// require an interactive accept or `--accept-changes`. `-y` does not
    /// satisfy this, because otherwise every systemd unit and CI job would
    /// carry permanent pre-consent to all future widening.
    Widened {
        ceiling_hash: Hash,
        /// What attenuation objected to. The decision and the message are
        /// the same value, so they cannot disagree.
        objection: Objection,
        change: Change,
    },
}

/// Why consent cannot be evaluated at all. Each stops start, by name.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConsentFailure {
    #[error("consent.json is for root {found}, but this root is {expected}; a consent file never travels, so one that does not match is not this root's")]
    RootIdMismatch { expected: Uuid7, found: Uuid7 },
    #[error("this root declares no ceiling; a root with a project.json states its caps or does not start (the wide default belongs to the no-root path)")]
    NoCeiling,
    #[error(transparent)]
    CeilingHash(#[from] project::CeilingHashError),
    #[error("consent.json binds the peer {binding}, and peer delivery is not supported in this build; remove the binding to start")]
    PeersNotSupported { binding: String },
}

/// The one function `start`, `dollup consent` and `dollup audit` all call.
///
/// Pure: the caller has already read both files. That is what makes audit
/// able to say what the runtime would do rather than approximate it — three
/// callers, one function, and no filesystem in here to diverge over.
pub fn check(
    project: &ProjectJson,
    consent: Option<&ConsentJson>,
) -> Result<ConsentCheck, ConsentFailure> {
    if project.caps.is_empty() {
        return Err(ConsentFailure::NoCeiling);
    }
    let ceiling_hash = project::ceiling_hash(project)?;

    let Some(consent) = consent else {
        return Ok(ConsentCheck::First { ceiling_hash });
    };
    if consent.root_id != project.root_id {
        return Err(ConsentFailure::RootIdMismatch {
            expected: project.root_id,
            found: consent.root_id,
        });
    }
    // Reserved, so it refuses rather than being ignored. A binding that
    // parsed and then did nothing would read, from the operator's side,
    // exactly like one that worked — which is the failure mode this whole
    // round is shaped against.
    if let Some(binding) = consent.peers.first() {
        return Err(ConsentFailure::PeersNotSupported {
            binding: binding.describe(),
        });
    }

    match consent.root_entry() {
        None => Ok(ConsentCheck::First { ceiling_hash }),
        Some(Accepted::All {
            accepted_against,
            accepted_at,
            ..
        }) => Ok(ConsentCheck::Blanket {
            accepted_at: *accepted_at,
            ceiling_changed: accepted_against != &ceiling_hash,
        }),
        Some(Accepted::Listed {
            ceiling_hash: accepted_hash,
            ceiling: accepted,
            ..
        }) => {
            if accepted_hash == &ceiling_hash {
                return Ok(ConsentCheck::Unchanged);
            }
            let declared = project::declared_ceiling(project);
            let change = Change::between(accepted, &declared);
            match widen_check_ceiling(&declared, accepted) {
                Ok(()) => Ok(ConsentCheck::Narrowed {
                    ceiling_hash,
                    change,
                }),
                Err(objection) => Ok(ConsentCheck::Widened {
                    ceiling_hash,
                    objection,
                    change,
                }),
            }
        }
    }
}

// depth: the widen relation, and why it is attenuation and nothing else

/// Does the new ceiling stay inside the accepted one?
///
/// **Attenuation, full stop.** Not a set difference, and not realm
/// coverage. Attenuation treats the two effects asymmetrically — allows may
/// only shrink, denies may only grow — so a new ceiling that *drops a deny*
/// adds nothing to the allow set, and an additive difference would call that
/// narrowing while it actually widens what the root may do. Silently. That
/// is the one hole this function exists to close, and
/// `dropping_a_deny_is_a_widening` below is the test that holds it shut.
///
/// New as child, accepted as parent: the question is whether the ceiling
/// the operator has *not* seen fits inside the one they accepted.
pub fn widen_check(new: &[Grant], accepted: &[Grant]) -> Result<(), Objection> {
    CapSet::root(accepted.to_vec())
        .attenuate(Principal("consent-widen-check".into()), new.to_vec())
        .map(|_| ())
        .map_err(Objection::Caps)
}

/// The whole relation over both halves of the ceiling: [`widen_check`] for
/// `caps`, and for `peers` an added role is a widening and a removed one is
/// a narrowing.
///
/// **Roles, and nothing finer.** A role that stays but whose `queues` or
/// `contract` changed is not a widening under this rule. That is deliberate
/// — the rule is one an operator can hold in their head — and it is a real
/// edge: the hash still moves, so such an edit lands on the silent
/// narrowing path and rewrites the entry. Anything that needs a prompt for
/// a changed queue list has to say so here, in this function, rather than
/// by being noticed somewhere else.
pub fn widen_check_ceiling(
    new: &DeclaredCeiling,
    accepted: &DeclaredCeiling,
) -> Result<(), Objection> {
    widen_check(&new.caps, &accepted.caps)?;
    let added: Vec<String> = new
        .peers
        .iter()
        .filter(|p| !accepted.peers.iter().any(|a| a.role == p.role))
        .map(|p| p.role.clone())
        .collect();
    if added.is_empty() {
        return Ok(());
    }
    Err(Objection::Peers { added })
}

/// What the widen relation objected to. Printed as the delta, because the
/// decision and the explanation must be one value — two computations over
/// the same edit can disagree, and the one that prints would be the one
/// nobody tested.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Objection {
    #[error("the ceiling widened: {0}")]
    Caps(drt_caps::AttenuationError),
    #[error("the ceiling widened: it declares {} nobody has consented to ({})", plural(added.len(), "a peer role", "peer roles"), added.join(", "))]
    Peers { added: Vec<String> },
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        many.to_string()
    }
}

/// The same edit described for a human: what appeared and what went away.
///
/// Descriptive only. It never decides anything — [`widen_check`] does that
/// — and it is here so the prompt can show an operator the edit rather than
/// only the objection to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub added: Vec<Grant>,
    pub removed: Vec<Grant>,
    /// Peer roles this ceiling gained, by name.
    pub peers_added: Vec<String>,
    /// Peer roles it lost.
    pub peers_removed: Vec<String>,
}

impl Change {
    pub fn between(accepted: &DeclaredCeiling, new: &DeclaredCeiling) -> Change {
        let same = |a: &Grant, b: &Grant| {
            a.effect == b.effect && a.capability == b.capability && a.scope == b.scope
        };
        let roles = |from: &DeclaredCeiling, not_in: &DeclaredCeiling| -> Vec<String> {
            from.peers
                .iter()
                .filter(|p| !not_in.peers.iter().any(|o| o.role == p.role))
                .map(|p| p.role.clone())
                .collect()
        };
        Change {
            added: new
                .caps
                .iter()
                .filter(|g| !accepted.caps.iter().any(|a| same(a, g)))
                .cloned()
                .collect(),
            removed: accepted
                .caps
                .iter()
                .filter(|g| !new.caps.iter().any(|n| same(n, g)))
                .cloned()
                .collect(),
            peers_added: roles(new, accepted),
            peers_removed: roles(accepted, new),
        }
    }

    /// One line per entry, `+`/`-` prefixed, for the prompt.
    pub fn lines(&self) -> Vec<String> {
        let describe = |g: &Grant, sign: char| {
            let effect = match g.effect {
                Effect::Grant => "",
                Effect::Deny => "deny ",
            };
            format!("{sign} {effect}{}", g.capability)
        };
        // A peer role reads as `peer db` rather than as a bare name, so a
        // reader scanning a delta does not have to know which half of the
        // ceiling a line came from.
        self.added
            .iter()
            .map(|g| describe(g, '+'))
            .chain(self.removed.iter().map(|g| describe(g, '-')))
            .chain(self.peers_added.iter().map(|r| format!("+ peer {r}")))
            .chain(self.peers_removed.iter().map(|r| format!("- peer {r}")))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.peers_added.is_empty()
            && self.peers_removed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Uuid7 {
        Uuid7::mint(1_757_707_440_000, [0x11; 10])
    }

    fn at() -> Timestamp {
        Timestamp::parse("2026-09-11T20:14:00Z").unwrap()
    }

    fn project(caps: Vec<Grant>) -> ProjectJson {
        ProjectJson {
            caps,
            ..ProjectJson::new(id())
        }
    }

    fn accepted(caps: Vec<Grant>) -> ConsentJson {
        let project = project(caps.clone());
        ConsentJson {
            root_id: id(),
            accepted: vec![Accepted::Listed {
                realm: Realm::root(),
                ceiling_hash: project::ceiling_hash(&project).unwrap(),
                ceiling: DeclaredCeiling::of_caps(caps),
                accepted_at: at(),
            }],
            signers: Vec::new(),
            peers: Vec::new(),
        }
    }

    fn peer(role: &str) -> crate::project::PeerDeclaration {
        crate::project::PeerDeclaration {
            role: role.into(),
            queues: vec!["query".into(), "result".into()],
            contract: "discofetch-db/1".into(),
        }
    }

    /// A descriptor declaring both halves, and a consent entry that has
    /// accepted exactly it.
    fn with_peers(
        caps: Vec<Grant>,
        peers: Vec<crate::project::PeerDeclaration>,
    ) -> (ProjectJson, ConsentJson) {
        let project = ProjectJson {
            caps,
            peers,
            ..ProjectJson::new(id())
        };
        let consent = ConsentJson {
            root_id: id(),
            accepted: vec![Accepted::Listed {
                realm: Realm::root(),
                ceiling_hash: project::ceiling_hash(&project).unwrap(),
                ceiling: project::declared_ceiling(&project),
                accepted_at: at(),
            }],
            signers: Vec::new(),
            peers: Vec::new(),
        };
        (project, consent)
    }

    fn binding(kind: PeerKind) -> PeerBinding {
        PeerBinding {
            peer: "db".into(),
            kind,
            queues: vec!["query".into(), "result".into()],
        }
    }

    fn a_key() -> PublicKey {
        crate::sign::SecretKey::generate([7u8; 32]).public_key()
    }

    /// Acceptance 3: a binding of either kind refuses at start by name, and
    /// the same file without it starts.
    ///
    /// Refused rather than ignored on purpose. A binding that parsed and
    /// then did nothing would look, from where the operator stands,
    /// exactly like one that worked — which is the shape of every failure
    /// this round is built against.
    #[test]
    fn a_peer_binding_of_either_kind_is_refused_by_name() {
        let caps = vec![Grant::grant("host:fs/*")];
        let project = project(caps.clone());

        for kind in [
            PeerKind::Root {
                root_id: Uuid7::mint(1_757_707_440_000, [0x44; 10]),
                public_key: a_key(),
            },
            PeerKind::Plugin {
                public_key: a_key(),
            },
        ] {
            let mut consent = accepted(caps.clone());
            consent.peers.push(binding(kind));

            let e = check(&project, Some(&consent)).expect_err("a bound peer refuses");
            let said = e.to_string();
            assert!(said.contains("db"), "the refusal names the role: {said}");
            assert!(
                said.contains("not supported in this build"),
                "and says why: {said}"
            );

            consent.peers.clear();
            assert_eq!(
                check(&project, Some(&consent)).unwrap(),
                ConsentCheck::Unchanged,
                "the same file without the binding starts"
            );
        }
    }

    /// The format is settled now so that whoever writes the first binding
    /// writes the final shape. Both spellings round-trip, and a root
    /// binding carries an id where a plugin cannot.
    #[test]
    fn both_binding_kinds_round_trip_through_their_settled_shape() {
        let root = binding(PeerKind::Root {
            root_id: Uuid7::mint(1_757_707_440_000, [0x44; 10]),
            public_key: a_key(),
        });
        let text = serde_json::to_string(&root).unwrap();
        assert!(text.contains("\"kind\":\"root\""), "{text}");
        assert!(text.contains("\"root_id\""), "{text}");
        assert_eq!(serde_json::from_str::<PeerBinding>(&text).unwrap(), root);

        let plugin = binding(PeerKind::Plugin {
            public_key: a_key(),
        });
        let text = serde_json::to_string(&plugin).unwrap();
        assert!(text.contains("\"kind\":\"plugin\""), "{text}");
        assert!(
            !text.contains("root_id"),
            "a plugin has no root id, so the shape cannot carry one: {text}"
        );
        assert_eq!(serde_json::from_str::<PeerBinding>(&text).unwrap(), plugin);
    }

    /// An unknown kind is refused by name rather than defaulted, which is
    /// what keeps a third kind from arriving silently.
    #[test]
    fn an_unknown_peer_kind_is_refused() {
        let e = serde_json::from_str::<PeerBinding>(
            r#"{"peer":"db","kind":"carrier-pigeon","public_key":"AAAA"}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("carrier-pigeon"), "{e}");
    }

    /// Acceptance 6, first half: a role that was not there before is reach
    /// into this root nobody agreed to, so it prompts.
    #[test]
    fn adding_a_peer_role_is_a_widening() {
        let caps = vec![Grant::grant("host:fs/*")];
        let (_, before) = with_peers(caps.clone(), vec![]);
        let (after, _) = with_peers(caps, vec![peer("db")]);

        let check = check(&after, Some(&before)).unwrap();
        let ConsentCheck::Widened {
            objection, change, ..
        } = check
        else {
            panic!("declaring a peer must prompt, got {check:?}");
        };
        assert_eq!(change.peers_added, ["db"]);
        assert!(change.added.is_empty(), "no cap moved");
        let said = objection.to_string();
        assert!(said.contains("db"), "the objection names the role: {said}");
        assert!(
            change.lines().contains(&"+ peer db".to_string()),
            "the delta reads as a peer, not a bare name: {:?}",
            change.lines()
        );
    }

    /// Acceptance 6, second half. Symmetric with a cap: giving something up
    /// never needs to be agreed to twice.
    #[test]
    fn removing_a_peer_role_is_silent() {
        let caps = vec![Grant::grant("host:fs/*")];
        let (_, before) = with_peers(caps.clone(), vec![peer("db"), peer("mail")]);
        let (after, _) = with_peers(caps, vec![peer("db")]);

        let check = check(&after, Some(&before)).unwrap();
        let ConsentCheck::Narrowed { change, .. } = check else {
            panic!("dropping a peer is a narrowing, got {check:?}");
        };
        assert_eq!(change.peers_removed, ["mail"]);
        assert!(change.peers_added.is_empty());
    }

    /// The migration, stated as a test: an entry written before `peers`
    /// existed hashes differently under the new preimage, and that must
    /// cost an operator nothing. Identical caps and no peers is a no-op
    /// edit, so it lands on the silent path and the entry is rewritten.
    #[test]
    fn an_entry_from_before_peers_is_rehashed_without_prompting() {
        let caps = vec![Grant::grant("host:fs/*"), Grant::deny("host:fs/remove")];
        let project = project(caps.clone());

        let stale = ConsentJson {
            root_id: id(),
            accepted: vec![Accepted::Listed {
                realm: Realm::root(),
                // What the old preimage produced: `caps` alone.
                ceiling_hash: crate::canon::hash_value(&serde_json::to_value(&caps).unwrap()),
                ceiling: DeclaredCeiling::of_caps(caps),
                accepted_at: at(),
            }],
            signers: Vec::new(),
            peers: Vec::new(),
        };

        let check = check(&project, Some(&stale)).unwrap();
        let ConsentCheck::Narrowed { change, .. } = check else {
            panic!("a re-hash with no edit must not prompt, got {check:?}");
        };
        assert!(
            change.is_empty(),
            "nothing actually changed, and the delta says so: {:?}",
            change.lines()
        );
    }

    /// The hole this rule leaves, asserted so it is a decision rather than
    /// a discovery: the widen relation is over role *names*, so editing an
    /// existing role's queues moves the hash without prompting. Whoever
    /// wants a prompt for it changes `widen_check_ceiling`, and this test
    /// is what will fail when they do.
    #[test]
    fn editing_an_existing_roles_queues_is_not_a_widening_today() {
        let caps = vec![Grant::grant("host:fs/*")];
        let (_, before) = with_peers(caps.clone(), vec![peer("db")]);
        let mut wider = peer("db");
        wider.queues.push("admin".into());
        let (after, _) = with_peers(caps, vec![wider]);

        assert_ne!(
            project::ceiling_hash(&after).unwrap(),
            match before.root_entry() {
                Some(Accepted::Listed { ceiling_hash, .. }) => ceiling_hash.clone(),
                _ => panic!("listed"),
            },
            "the queue list is inside the hashed ceiling"
        );
        let check = check(&after, Some(&before)).unwrap();
        assert!(
            matches!(check, ConsentCheck::Narrowed { .. }),
            "today this is silent, got {check:?}"
        );
    }

    #[test]
    fn a_matching_hash_is_silent() {
        let caps = vec![Grant::grant("host:fs/*")];
        assert_eq!(
            check(&project(caps.clone()), Some(&accepted(caps))).unwrap(),
            ConsentCheck::Unchanged
        );
    }

    #[test]
    fn no_consent_file_is_a_first_acceptance() {
        let project = project(vec![Grant::grant("host:fs/*")]);
        assert!(matches!(
            check(&project, None).unwrap(),
            ConsentCheck::First { .. }
        ));
        // A file with no entry at the root realm is the same situation.
        assert!(matches!(
            check(&project, Some(&ConsentJson::new(id()))).unwrap(),
            ConsentCheck::First { .. }
        ));
    }

    #[test]
    fn removing_an_allow_is_silent() {
        let before = accepted(vec![Grant::grant("host:fs/*"), Grant::grant("host:time")]);
        let after = project(vec![Grant::grant("host:fs/*")]);
        let check = check(&after, Some(&before)).unwrap();
        assert!(matches!(check, ConsentCheck::Narrowed { .. }), "{check:?}");
    }

    #[test]
    fn adding_an_allow_prompts() {
        let before = accepted(vec![Grant::grant("host:fs/read")]);
        let after = project(vec![
            Grant::grant("host:fs/read"),
            Grant::grant("host:exec/run"),
        ]);
        let ConsentCheck::Widened {
            change, objection, ..
        } = check(&after, Some(&before)).unwrap()
        else {
            panic!("adding a capability is a widening");
        };
        assert!(
            objection.to_string().contains("host:exec/run"),
            "{objection}"
        );
        assert_eq!(change.lines(), ["+ host:exec/run"]);
    }

    /// The hole that an additive set difference would have left open, and
    /// the reason `widen_check` is attenuation: dropping a deny adds nothing
    /// to the allow set while widening what the root may do.
    #[test]
    fn dropping_a_deny_is_a_widening() {
        let before = accepted(vec![
            Grant::grant("host:fs/*"),
            Grant::deny("host:fs/remove"),
        ]);
        let after = project(vec![Grant::grant("host:fs/*")]);

        let change = Change::between(
            &DeclaredCeiling::of_caps(vec![
                Grant::grant("host:fs/*"),
                Grant::deny("host:fs/remove"),
            ]),
            &project::declared_ceiling(&after),
        );
        assert!(
            change.added.is_empty(),
            "nothing was added, which is exactly why a set difference would have called this narrowing"
        );

        let check = check(&after, Some(&before)).unwrap();
        let ConsentCheck::Widened { objection, .. } = check else {
            panic!("dropping a deny must prompt, got {check:?}");
        };
        assert!(
            objection.to_string().contains("host:fs/remove"),
            "{objection}"
        );
    }

    /// consent.md acceptance 1's mixed case: one allow dropped and another
    /// added in one edit. Any addition decides it, whatever was also
    /// removed, which is what makes this one check rather than two branches.
    #[test]
    fn a_mixed_edit_cannot_fall_through() {
        let before = accepted(vec![
            Grant::grant("host:fs/read"),
            Grant::grant("host:time"),
        ]);
        let after = project(vec![
            Grant::grant("host:fs/read"),
            Grant::grant("host:rest/get"),
        ]);
        let ConsentCheck::Widened { change, .. } = check(&after, Some(&before)).unwrap() else {
            panic!("an addition decides it even beside a removal");
        };
        assert_eq!(change.added.len(), 1);
        assert_eq!(change.removed.len(), 1);
    }

    #[test]
    fn blanket_consent_never_prompts_and_says_when_the_ceiling_moved() {
        let project = project(vec![
            Grant::grant("host:fs/*"),
            Grant::grant("host:exec/run"),
        ]);
        let consent = ConsentJson {
            root_id: id(),
            accepted: vec![Accepted::All {
                realm: Realm::root(),
                accepted_against: project::caps_hash(&[Grant::grant("host:fs/*")]).unwrap(),
                accepted_at: at(),
            }],
            signers: Vec::new(),
            peers: Vec::new(),
        };
        assert_eq!(
            check(&project, Some(&consent)).unwrap(),
            ConsentCheck::Blanket {
                accepted_at: at(),
                ceiling_changed: true
            },
            "the ceiling moved, the entry does not care, and audit is told"
        );
    }

    #[test]
    fn a_root_id_mismatch_is_a_named_failure() {
        let other = Uuid7::mint(1_757_707_441_000, [0x22; 10]);
        let mut consent = accepted(vec![Grant::grant("host:fs/*")]);
        consent.root_id = other;
        let e = check(&project(vec![Grant::grant("host:fs/*")]), Some(&consent)).unwrap_err();
        assert!(matches!(e, ConsentFailure::RootIdMismatch { .. }), "{e}");
        assert!(e.to_string().contains(&other.to_string()), "{e}");
    }

    #[test]
    fn a_root_with_no_ceiling_does_not_start() {
        let e = check(&project(vec![]), None).unwrap_err();
        assert!(matches!(e, ConsentFailure::NoCeiling), "{e}");
    }

    /// The modes are different shapes on the wire, and an unknown mode is
    /// refused by name.
    #[test]
    fn the_two_modes_are_distinct_on_the_wire() {
        let json = serde_json::to_string(&accepted(vec![Grant::grant("host:fs/*")])).unwrap();
        assert!(json.contains(r#""mode":"listed""#), "{json}");
        assert!(
            json.contains(r#""ceiling""#),
            "listed carries its ceiling verbatim"
        );
        assert!(!json.contains("accepted_against"), "that field is all's");

        let unknown = r#"{"root_id":"0192f0c1-8000-7000-8000-00000000abcd","accepted":[{"mode":"whatever","realm":"operator"}]}"#;
        let e = serde_json::from_str::<ConsentJson>(unknown).unwrap_err();
        assert!(e.to_string().contains("whatever"), "{e}");
    }

    #[test]
    fn a_listed_entry_derives_its_realms_from_its_ceiling() {
        let entry = accepted(vec![
            Grant::grant("host:fs/*"),
            Grant::grant("host:rest/get"),
        ]);
        let realms = entry.root_entry().unwrap().realms(&RealmRegistry::new());
        assert_eq!(
            realms.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            ["operator.fs", "operator.rest.get"]
        );
    }

    #[test]
    fn round_trips_through_json() {
        let before = accepted(vec![Grant::grant("host:fs/*")]);
        let text = serde_json::to_string(&before).unwrap();
        assert_eq!(serde_json::from_str::<ConsentJson>(&text).unwrap(), before);
    }
}
