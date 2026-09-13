//! Realms: the consent noun (consent.md §3).
//!
//! A realm is a dotted policy path under `operator`. It is **not**
//! [`drt_caps::Scope`], which is what a grant applies to — a directory, a
//! CIDR, a key. Different kind, and they meet in exactly one place (the
//! approval verification chain), so they deliberately do not share a name.
//! `TurnConfig.realm` elsewhere in this crate is the TURN authentication
//! realm and is a third, unrelated use; the JSON never collides because it
//! is nested under `turn`, and the comment at that field says so.
//!
//! ## surface block
//!
//! - Entry points: [`Realm::parse`], text to a validated realm;
//!   [`Realm::covers`], the coverage relation, which is the whole of the
//!   path mechanics; [`RealmRegistry`], cap-to-realm for a set of grants.
//! - Configurable: [`ROOT`], the root realm's name. One value, because
//!   consent.md fixes it.
//! - Fan-out: [`RealmOf`] is the per-family seam — one implementation per
//!   capability family, registered by capability pattern, with
//!   [`declare_families`] naming the ones this build declares. [`Peer`] is
//!   the only declared family; [`Structural`] is the default for every
//!   family that has not declared one.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use drt_caps::{Effect, Grant};

/// The root realm. Consent at `operator` covers everything, forever, which
/// is what `mode: "all"` is.
pub const ROOT: &str = "operator";

/// A dotted policy path under [`ROOT`].
///
/// Validated on the way in rather than trusted, because the failure mode
/// of an invalid realm is silence: `opereator.net` is a perfectly good
/// string that covers nothing and is covered by nothing, so a typo in
/// `consent.json` would read as "this signer is authorized for nothing"
/// rather than as a mistake.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Realm(String);

impl Realm {
    /// The root realm, `operator`.
    pub fn root() -> Realm {
        Realm(ROOT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Does consent at `self` cover `other`?
    ///
    /// Equality, or `other` lying strictly beneath `self` — which means the
    /// separator is part of the test. **This must not be
    /// [`drt_caps::implies`]**: that is byte-prefix matching with a
    /// trailing `*` and no separator awareness, so pointed at dotted paths
    /// it would have `operator.net` cover `operator.network`. That is a
    /// widening carrying a valid signature, which is exactly what
    /// consent.md §7's invariant exists to prevent. The test below is that
    /// pair.
    pub fn covers(&self, other: &Realm) -> bool {
        if self.0 == other.0 {
            return true;
        }
        other
            .0
            .strip_prefix(&self.0)
            .is_some_and(|rest| rest.starts_with('.'))
    }

    /// Is any realm in `set` covering `self`? The question both §7 step 2
    /// (a signer's `realms`) and §7 step 3 (the ceiling's realms) ask.
    pub fn covered_by_any(&self, set: &[Realm]) -> bool {
        set.iter().any(|r| r.covers(self))
    }

    /// Text to a realm, or why not.
    pub fn parse(text: &str) -> Result<Realm, BadRealm> {
        let mut segments = text.split('.');
        // The root segment is checked by name so a misspelling is a
        // refusal rather than a realm that quietly relates to nothing.
        match segments.next() {
            Some(ROOT) => {}
            _ => {
                return Err(BadRealm::NotUnderRoot {
                    realm: text.to_string(),
                })
            }
        }
        for segment in text.split('.') {
            if segment.is_empty() {
                return Err(BadRealm::EmptySegment {
                    realm: text.to_string(),
                });
            }
            if let Some(bad) = segment
                .chars()
                .find(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '_' && *c != '-')
            {
                return Err(BadRealm::BadCharacter {
                    realm: text.to_string(),
                    character: bad,
                });
            }
        }
        Ok(Realm(text.to_string()))
    }
}

/// Why a realm path is not one. Each is a named failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadRealm {
    #[error("'{realm}' is not a realm; every realm lives under '{ROOT}'")]
    NotUnderRoot { realm: String },
    #[error("'{realm}' has an empty segment; realms are dot-separated names")]
    EmptySegment { realm: String },
    #[error("'{realm}': '{character}' is not allowed in a realm; use lowercase letters, digits, '_' and '-'")]
    BadCharacter { realm: String, character: char },
}

impl fmt::Display for Realm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Realm> for String {
    fn from(r: Realm) -> String {
        r.0
    }
}

impl TryFrom<String> for Realm {
    type Error = BadRealm;
    fn try_from(s: String) -> Result<Realm, BadRealm> {
        Realm::parse(&s)
    }
}

// depth: cap to realm, the one place the two types meet

/// One capability family's answer to "which realm does this grant live
/// under?" (consent.md §3).
///
/// The seam, not the taxonomy. consent.md is explicit that the taxonomy is
/// not this session's to invent, so what is here is the signature, the
/// registry that dispatches to it, and one structural default. A family
/// that wants a different answer registers its own and nothing else
/// changes.
pub trait RealmOf: Send + Sync {
    fn realm_of(&self, grant: &Grant) -> Option<Realm>;
}

/// The default: the capability name, mechanically, as a realm path.
///
/// `host:fs/read` becomes `operator.fs.read`; `host:rest/*` becomes
/// `operator.rest`. A trailing `*` names the family rather than a leaf,
/// which is right — a grant covering a family should map to the realm that
/// covers that family's leaves.
///
/// This is a placeholder with a mechanical rule, **not** a designed
/// taxonomy, and it is documented as such so nobody reads a decision into
/// it. It exists because §7 step 3 has to work in v1: without some mapping
/// there is no way to ask whether a request's realm is inside a ceiling
/// expressed in caps.
pub struct Structural;

impl RealmOf for Structural {
    fn realm_of(&self, grant: &Grant) -> Option<Realm> {
        let name = grant
            .capability
            .strip_prefix("host:")
            .unwrap_or(&grant.capability);
        let mut path = String::from(ROOT);
        for segment in name.split('/') {
            // A trailing `*` is the family, so the wildcard segment is
            // dropped rather than spelled into the path.
            let segment = segment.trim_end_matches('*');
            if segment.is_empty() {
                continue;
            }
            path.push('.');
            // `_` and `-` survive; anything else a capability name can
            // carry that a realm cannot becomes `_`, because refusing here
            // would make an ordinary grant unmappable.
            for c in segment.chars() {
                match c {
                    c if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' => {
                        path.push(c)
                    }
                    c if c.is_ascii_uppercase() => path.push(c.to_ascii_lowercase()),
                    _ => path.push('_'),
                }
            }
        }
        Realm::parse(&path).ok()
    }
}

// depth: the peer family's realm, declared rather than inherited
//
// `Structural` would already map `host:peer/db` to `operator.peer.db`, so
// this impl changes no answer today. It exists so the mapping is a
// decision with a name on it: peers are the one family whose realm reaches
// an operator through two files (a role declared in `project.json`, a
// binding in `consent.json`), and inheriting that from a mechanical
// fallback would make it look accidental. If the taxonomy ever moves the
// family, it moves here and the tests below say what moved.

/// `host:peer/<role>` lives under `operator.peer.<role>`, and
/// `host:peer/*` under `operator.peer`.
pub struct Peer;

impl RealmOf for Peer {
    fn realm_of(&self, grant: &Grant) -> Option<Realm> {
        Structural.realm_of(grant)
    }
}

/// The families this build declares, for a caller assembling a registry.
///
/// One function so that drt and dollup cannot disagree about which
/// families are mapped — the same reason `RESERVED` is one list.
pub fn declare_families(registry: &mut RealmRegistry) {
    registry.declare(drt_caps::PEER_FAMILY, Peer);
}

/// Capability pattern to realm mapping, the shape `drt_caps::ScopeRegistry`
/// already uses for scope types: families declare, and one lookup answers
/// for any grant.
pub struct RealmRegistry {
    families: BTreeMap<String, Box<dyn RealmOf>>,
    fallback: Box<dyn RealmOf>,
}

impl Default for RealmRegistry {
    fn default() -> Self {
        RealmRegistry {
            families: BTreeMap::new(),
            fallback: Box::new(Structural),
        }
    }
}

impl RealmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a family's mapping for a capability name or trailing-`*`
    /// pattern, exactly as a scope type is declared.
    pub fn declare(&mut self, capability: impl Into<String>, of: impl RealmOf + 'static) {
        self.families.insert(capability.into(), Box::new(of));
    }

    /// The realm one grant lives under.
    pub fn realm_of(&self, grant: &Grant) -> Option<Realm> {
        for (pattern, of) in &self.families {
            if drt_caps::implies(pattern, &grant.capability) {
                return of.realm_of(grant);
            }
        }
        self.fallback.realm_of(grant)
    }

    /// The realms a ceiling covers: every **allow** mapped, deduplicated,
    /// and with any realm a sibling already covers collapsed away.
    ///
    /// Denies are not mapped. A deny narrows what a grant reaches, and a
    /// realm list is the set consent *covers*; folding a deny in here would
    /// make the list mean two things at once. §7 step 3 asks whether a
    /// request's realm is inside the ceiling, and the allows are what
    /// answer that; whether a specific call is then denied is
    /// [`drt_caps::CapSet::holds`]'s question, at the call.
    pub fn realms_of(&self, caps: &[Grant]) -> Vec<Realm> {
        let mut out: Vec<Realm> = caps
            .iter()
            .filter(|g| g.effect == Effect::Grant)
            .filter_map(|g| self.realm_of(g))
            .collect();
        out.sort();
        out.dedup();
        collapse(out)
    }
}

/// Drop any realm another in the list already covers — consent.md §5's
/// collapse, which is also what keeps a derived list short enough to print.
///
/// Lossy toward more permission, deliberately and per §5: with
/// `operator.net` present, `operator.net.domains` disappears and nobody can
/// tell who needed it. The uncollapsed view with attribution is a named
/// seam, not built.
pub fn collapse(mut realms: Vec<Realm>) -> Vec<Realm> {
    realms.sort();
    realms.dedup();
    // Sorted order puts a covering realm before everything it covers, so
    // one forward pass keeping whatever the last kept realm does not cover
    // is the whole algorithm.
    let mut out: Vec<Realm> = Vec::with_capacity(realms.len());
    for realm in realms {
        if out.last().is_some_and(|kept| kept.covers(&realm)) {
            continue;
        }
        out.push(realm);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pair consent.md names as the test. A byte-prefix matcher gets
    /// this wrong, which is why `covers` is not `drt_caps::implies`.
    #[test]
    fn net_does_not_cover_network() {
        let net = Realm::parse("operator.net").unwrap();
        let network = Realm::parse("operator.network").unwrap();
        let domains = Realm::parse("operator.net.domains").unwrap();

        assert!(net.covers(&domains), "a realm covers its descendants");
        assert!(net.covers(&net), "and itself");
        assert!(
            !net.covers(&network),
            "operator.net must not cover operator.network"
        );
        assert!(!domains.covers(&net), "coverage does not run upward");

        // The relation this must not be implemented with, shown failing.
        assert!(
            drt_caps::implies("operator.net*", "operator.network"),
            "byte-prefix matching is what would have been wrong"
        );
    }

    #[test]
    fn the_root_realm_covers_everything() {
        let root = Realm::root();
        for path in [
            "operator",
            "operator.net",
            "operator.net.domains",
            "operator.fs.read",
        ] {
            assert!(root.covers(&Realm::parse(path).unwrap()), "{path}");
        }
    }

    #[test]
    fn a_realm_is_parsed_not_trusted() {
        assert!(matches!(
            Realm::parse("opereator.net"),
            Err(BadRealm::NotUnderRoot { .. })
        ));
        assert!(matches!(
            Realm::parse("operator..net"),
            Err(BadRealm::EmptySegment { .. })
        ));
        assert!(matches!(
            Realm::parse("operator.Net"),
            Err(BadRealm::BadCharacter { .. })
        ));
        assert!(Realm::parse("operator.net.domains-v2").is_ok());
    }

    #[test]
    fn the_structural_default_maps_a_family_to_a_realm() {
        let registry = RealmRegistry::new();
        let realm = |cap: &str| registry.realm_of(&Grant::grant(cap)).unwrap().to_string();
        assert_eq!(realm("host:fs/read"), "operator.fs.read");
        assert_eq!(realm("host:rest/*"), "operator.rest");
        assert_eq!(realm("host:*"), "operator");
        assert_eq!(realm("lifecycle"), "operator.lifecycle");
    }

    #[test]
    fn a_family_can_declare_its_own_mapping() {
        struct Fixed;
        impl RealmOf for Fixed {
            fn realm_of(&self, _: &Grant) -> Option<Realm> {
                Some(Realm::parse("operator.net.domains").unwrap())
            }
        }
        let mut registry = RealmRegistry::new();
        registry.declare("host:rest/*", Fixed);
        assert_eq!(
            registry.realm_of(&Grant::grant("host:rest/get")).unwrap(),
            Realm::parse("operator.net.domains").unwrap()
        );
        // Undeclared families still answer through the default.
        assert_eq!(
            registry.realm_of(&Grant::grant("host:fs/read")).unwrap(),
            Realm::parse("operator.fs.read").unwrap()
        );
    }

    /// A ceiling's realms are its allows, collapsed. The deny is not in the
    /// list: it narrows a call, it does not describe what consent covers.
    #[test]
    fn realms_of_a_ceiling_are_its_allows_collapsed() {
        let registry = RealmRegistry::new();
        let realms = registry.realms_of(&[
            Grant::grant("host:fs/*"),
            Grant::grant("host:fs/read"),
            Grant::grant("host:rest/get"),
            Grant::deny("host:fs/remove"),
        ]);
        assert_eq!(
            realms.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            ["operator.fs", "operator.rest.get"],
            "fs.read collapses into fs; the deny is not a realm"
        );
    }

    #[test]
    fn collapse_keeps_the_covering_realm() {
        let realms = collapse(vec![
            Realm::parse("operator.net.domains").unwrap(),
            Realm::parse("operator.net").unwrap(),
            Realm::parse("operator.network").unwrap(),
        ]);
        assert_eq!(
            realms.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            ["operator.net", "operator.network"],
            "net swallows net.domains and leaves network alone"
        );
    }
}

#[cfg(test)]
mod peer_realm_tests {
    use super::*;

    /// Seam 5 and amendment 4: the peer family maps to its realm like any
    /// other family, so consent covers a peer grant the same way.
    #[test]
    fn a_peer_grant_lives_under_operator_peer() {
        let mut registry = RealmRegistry::new();
        declare_families(&mut registry);

        assert_eq!(
            registry.realm_of(&Grant::grant(drt_caps::peer_capability("db"))),
            Some(Realm::parse("operator.peer.db").unwrap())
        );
        assert_eq!(
            registry.realm_of(&Grant::grant("host:peer/*")),
            Some(Realm::parse("operator.peer").unwrap())
        );
    }

    /// Consent at `operator.peer` covers every role, and consent at one
    /// role does not cover another. That is the realm tree doing its
    /// ordinary job, asserted here because a peer grant is the case an
    /// operator is most likely to reason about by hand.
    #[test]
    fn one_roles_realm_does_not_cover_another() {
        let family = Realm::parse("operator.peer").unwrap();
        let db = Realm::parse("operator.peer.db").unwrap();
        let mail = Realm::parse("operator.peer.mail").unwrap();

        assert!(family.covers(&db) && family.covers(&mail));
        assert!(!db.covers(&mail) && !mail.covers(&db));
    }

    /// A ceiling's realm list folds peer roles in like anything else.
    #[test]
    fn peer_grants_reach_the_ceilings_realm_list() {
        let mut registry = RealmRegistry::new();
        declare_families(&mut registry);
        let realms = registry.realms_of(&[
            Grant::grant("host:fs/read"),
            Grant::grant(drt_caps::peer_capability("db")),
        ]);
        assert!(
            realms.contains(&Realm::parse("operator.peer.db").unwrap()),
            "{realms:?}"
        );
    }
}
