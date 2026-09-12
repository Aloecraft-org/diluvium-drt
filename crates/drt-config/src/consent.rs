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
//!   [`widen_check`], the relation that decides silent-or-prompt;
//!   [`Change`], the same edit described for a human to read.
//! - Configurable: nothing. The modes are [`Accepted`]'s variants.
//! - Fan-out: [`ConsentCheck`] is every answer `check` can give, and
//!   [`ConsentFailure`] every way it refuses. `start` acts on one arm each;
//!   audit renders them.

use serde::{Deserialize, Serialize};

use drt_caps::{CapSet, Effect, Grant, Principal};

use crate::canon::Hash;
use crate::id::Uuid7;
use crate::project::{self, ProjectJson};
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
        /// The accepted caps, stored **verbatim** beside their hash.
        ///
        /// Without this an entry can only say "changed", never which
        /// direction, and §4's delta cannot be computed at all — a hash is
        /// not invertible. Small, operator-owned, travels nowhere.
        ceiling: Vec<Grant>,
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
            Accepted::Listed { ceiling, .. } => registry.realms_of(ceiling),
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
            let change = Change::between(accepted, &project.caps);
            match widen_check(&project.caps, accepted) {
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
        .map_err(Objection)
}

/// What attenuation objected to. Printed as the delta, because the decision
/// and the explanation must be one value — two computations over the same
/// edit can disagree, and the one that prints would be the one nobody
/// tested.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("the ceiling widened: {0}")]
pub struct Objection(pub drt_caps::AttenuationError);

/// The same edit described for a human: what appeared and what went away.
///
/// Descriptive only. It never decides anything — [`widen_check`] does that
/// — and it is here so the prompt can show an operator the edit rather than
/// only the objection to it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub added: Vec<Grant>,
    pub removed: Vec<Grant>,
}

impl Change {
    pub fn between(accepted: &[Grant], new: &[Grant]) -> Change {
        let same = |a: &Grant, b: &Grant| {
            a.effect == b.effect && a.capability == b.capability && a.scope == b.scope
        };
        Change {
            added: new
                .iter()
                .filter(|g| !accepted.iter().any(|a| same(a, g)))
                .cloned()
                .collect(),
            removed: accepted
                .iter()
                .filter(|g| !new.iter().any(|n| same(n, g)))
                .cloned()
                .collect(),
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
        self.added
            .iter()
            .map(|g| describe(g, '+'))
            .chain(self.removed.iter().map(|g| describe(g, '-')))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
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
                ceiling: caps,
                accepted_at: at(),
            }],
            signers: Vec::new(),
        }
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
            &[Grant::grant("host:fs/*"), Grant::deny("host:fs/remove")],
            &after.caps,
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
