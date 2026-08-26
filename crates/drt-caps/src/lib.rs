//! The capability grammar (SPEC.md §6): `effect × capability × scope`,
//! pattern match, attenuation check, and — what the C layer never had —
//! provenance.
//!
//! The name grammar is exactly `dvs_holds`/`dvs_may_grant` from
//! `aloecraft-org/diluvium` `src/dvs.c`, and stays differentially testable
//! against them: with no `deny` grants anywhere, [`CapSet::holds`] and
//! [`CapSet::may_grant`] reduce to those two functions. `deny` and scopes are
//! the Capabilities.md extension DRT implements first.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Does `held` imply `want`?
///
/// Exact match, or a trailing `*` that covers a prefix — the one pattern the
/// design already shows (`queue:work/*`, `host:fs/*`) and nothing more. A `*`
/// in the middle is not a pattern, it is a literal, because inventing a glob
/// here would be designing the future work rather than leaving room for it.
///
/// A bare `"*"` is a literal name matching only itself, never a wildcard: a
/// zero-length prefix would match everything, and worse, it could be reached
/// by *attenuation* (a parent holding `"**"` implies the want `"*"`), letting
/// a grant widen — a break in the model rather than a sharp edge. Ported from
/// `implies()` in `dvs.c`; keep the two in agreement until diluvium deletes
/// its copy.
pub fn implies(held: &str, want: &str) -> bool {
    let held = held.as_bytes();
    let want = want.as_bytes();
    let n = held.len();
    if n > 1 && held[n - 1] == b'*' {
        let prefix = &held[..n - 1];
        return want.len() >= prefix.len() && &want[..prefix.len()] == prefix;
    }
    held == want
}

/// The capability name a hostcall's `call` field is gated by: `host:` plus
/// the call name, so `host:fs/*` covers `fs/read` the way `queue:work/*`
/// covers queue names (doc/Hostcall.md).
pub fn call_capability(call: &str) -> String {
    format!("host:{call}")
}

/// Grant or deny. `dvs.c` has grants only; `deny` is the Capabilities.md
/// extension, and attenuation treats it asymmetrically: allows may only
/// shrink, denies may only grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Grant,
    Deny,
}

/// A scope: what a grant *applies to*. Opaque here — a tagged value each
/// capability validates via its declared [`ScopeType`], not free
/// polymorphism. A directory, a CIDR, a key, a port range are all scopes;
/// which shapes qualify a given capability is that capability's declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(pub rmpv::Value);

/// A grant is `effect × capability × scope` (Capabilities.md §1). Scope is
/// optional with a sane default on purpose: mandatory scope on every grant is
/// friction, and friction is what gets hacked around.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    #[serde(default = "default_effect")]
    pub effect: Effect,
    /// The capability name or trailing-`*` pattern: `host:time`, `host:fs/*`,
    /// `queue:work/*`, `lifecycle`.
    pub capability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
}

fn default_effect() -> Effect {
    Effect::Grant
}

impl Grant {
    // Named for the effect it carries, like `deny` below; the type sharing
    // the word is the point, not an accident the lint suspects.
    #[allow(clippy::self_named_constructors)]
    pub fn grant(capability: impl Into<String>) -> Self {
        Grant {
            effect: Effect::Grant,
            capability: capability.into(),
            scope: None,
        }
    }

    pub fn deny(capability: impl Into<String>) -> Self {
        Grant {
            effect: Effect::Deny,
            capability: capability.into(),
            scope: None,
        }
    }

    pub fn with_scope(mut self, scope: rmpv::Value) -> Self {
        self.scope = Some(Scope(scope));
        self
    }
}

/// Who a capability set belongs to or was granted by: an instance, the
/// process root, an SSH principal — all nodes in one provenance tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Principal(pub String);

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an attenuation was refused. Worded to be surfaced directly: a refusal
/// names the grant, never a bare `denied`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttenuationError {
    #[error(
        "'{capability}' is not covered by any grant the parent holds; a grant may only narrow"
    )]
    NotHeldByParent { capability: String },
    #[error("the parent denies '{capability}' and the child does not; a deny may only grow")]
    DenyDropped { capability: String },
}

/// An instance's capability set with its provenance: who granted it,
/// attenuated from what, back to the process root. Inspectable means
/// provable (SPEC.md §6).
#[derive(Debug, Clone)]
pub struct CapSet {
    grants: Vec<Grant>,
    provenance: Provenance,
}

#[derive(Debug, Clone)]
pub enum Provenance {
    /// The process root: the ceiling, granted by config rather than a parent.
    Root,
    Granted {
        by: Principal,
        from: Arc<CapSet>,
    },
}

impl CapSet {
    /// The root of a provenance tree — the ceiling the process config sets.
    pub fn root(grants: Vec<Grant>) -> Arc<Self> {
        Arc::new(CapSet {
            grants,
            provenance: Provenance::Root,
        })
    }

    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// Does this set hold `want`? At least one grant implies it and no deny
    /// implies it. With no denies present this is `dvs_holds`.
    pub fn holds(&self, want: &str) -> bool {
        let denied = self
            .grants
            .iter()
            .any(|g| g.effect == Effect::Deny && implies(&g.capability, want));
        !denied
            && self
                .grants
                .iter()
                .any(|g| g.effect == Effect::Grant && implies(&g.capability, want))
    }

    /// Attenuation only, no exceptions: granting is holding. A parent may
    /// pass on anything it holds, exactly or narrowed, and nothing else —
    /// this is what makes the privilege hierarchy structural instead of
    /// conventional. Identical to `dvs_may_grant`.
    pub fn may_grant(&self, capability: &str) -> bool {
        self.holds(capability)
    }

    /// Derive a child set, checking attenuation — the only rule in the
    /// system, applied identically whether the parent is the process root or
    /// another instance. Every child `grant` must be covered by a parent
    /// grant ([`CapSet::may_grant`] on its name or pattern), and every parent
    /// `deny` must still be covered by a child deny.
    pub fn attenuate(
        self: &Arc<Self>,
        by: Principal,
        grants: Vec<Grant>,
    ) -> Result<Arc<CapSet>, AttenuationError> {
        for g in grants.iter().filter(|g| g.effect == Effect::Grant) {
            if !self.may_grant(&g.capability) {
                return Err(AttenuationError::NotHeldByParent {
                    capability: g.capability.clone(),
                });
            }
        }
        for parent_deny in self.grants.iter().filter(|g| g.effect == Effect::Deny) {
            let covered = grants
                .iter()
                .filter(|g| g.effect == Effect::Deny)
                .any(|child| implies(&child.capability, &parent_deny.capability));
            if !covered {
                return Err(AttenuationError::DenyDropped {
                    capability: parent_deny.capability.clone(),
                });
            }
        }
        Ok(Arc::new(CapSet {
            grants,
            provenance: Provenance::Granted {
                by,
                from: Arc::clone(self),
            },
        }))
    }

    /// The provenance chain, this set first, back to the process root. Each
    /// step is (granting principal, the set it attenuated from).
    pub fn chain(&self) -> impl Iterator<Item = (&Principal, &CapSet)> {
        let mut cursor = Some(self);
        std::iter::from_fn(move || match &cursor.take()?.provenance {
            Provenance::Root => None,
            Provenance::Granted { by, from } => {
                cursor = Some(from);
                Some((by, from.as_ref()))
            }
        })
    }
}

/// A capability's scope-type declaration (SPEC.md §5): defining a capability
/// includes defining what scopes qualify it. Validation runs **at startup, by
/// name** — a malformed or ill-scoped grant is a named refusal then, never a
/// mystifying `denied` at first call.
pub trait ScopeType: Send + Sync {
    /// e.g. "a directory path", for the startup error message.
    fn describe(&self) -> &str;
    fn validate(&self, scope: Option<&Scope>) -> Result<(), String>;
}

impl ScopeType for Box<dyn ScopeType> {
    fn describe(&self) -> &str {
        (**self).describe()
    }
    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        (**self).validate(scope)
    }
}

/// A scope-type that accepts an absent scope and nothing else — for
/// capabilities that have nothing to scope (e.g. `host:time`).
pub struct NoScope;

impl ScopeType for NoScope {
    fn describe(&self) -> &str {
        "no scope"
    }
    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        match scope {
            None => Ok(()),
            Some(_) => Err("takes no scope".into()),
        }
    }
}

/// A scope that does not fit the capability it was written for. Raised
/// from two places — validating a config's grants, and wiring a connector —
/// so the wording names the capability and the fix rather than guessing
/// which of the two the reader was looking at.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{capability}': {detail} (expected {expected})")]
pub struct ScopeError {
    pub capability: String,
    pub expected: String,
    pub detail: String,
}

/// The scope-type registry: capability pattern → declared scope-type.
/// Connectors register their declarations here; [`ScopeRegistry::validate`]
/// is the startup gate.
#[derive(Default)]
pub struct ScopeRegistry {
    types: BTreeMap<String, Box<dyn ScopeType>>,
}

impl ScopeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare the scope-type for a capability name or trailing-`*` pattern.
    pub fn declare(&mut self, capability: impl Into<String>, ty: impl ScopeType + 'static) {
        self.types.insert(capability.into(), Box::new(ty));
    }

    /// Validate every grant in a config against the declarations. Grants for
    /// capabilities with no declaration pass — the registry gates shape, not
    /// existence; whether a capability is *wired* is the connector registry's
    /// question.
    pub fn validate(&self, grants: &[Grant]) -> Result<(), ScopeError> {
        for g in grants {
            for (pattern, ty) in &self.types {
                if implies(pattern, &g.capability) {
                    ty.validate(g.scope.as_ref()).map_err(|detail| ScopeError {
                        capability: g.capability.clone(),
                        expected: ty.describe().to_string(),
                        detail,
                    })?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cases `implies()` in dvs.c is written around; keep in lockstep.
    #[test]
    fn implies_matches_dvs_semantics() {
        assert!(implies("queue:work/jobs", "queue:work/jobs"));
        assert!(!implies("queue:work/jobs", "queue:work"));
        assert!(implies("queue:work/*", "queue:work/jobs"));
        assert!(implies("queue:work/*", "queue:work/"));
        assert!(implies("host:fs/*", "host:fs/read"));
        assert!(!implies("host:fs/read", "host:fs/*"));
        // A pattern implies a narrower pattern: attenuation over patterns.
        assert!(implies("host:fs/*", "host:fs/read/*"));
        // The bare "*" is a literal, matching only itself...
        assert!(!implies("*", "host:time"));
        assert!(implies("*", "*"));
        // ...and "**" is a prefix pattern over names starting with '*', which
        // does imply the literal "*" — the widening dvs.c documents and
        // forecloses by making "*" powerless, not by special-casing "**".
        assert!(implies("**", "*"));
        assert!(!implies("", "anything"));
        assert!(implies("a*", "abc"));
    }

    #[test]
    fn holds_reduces_to_dvs_holds_without_denies() {
        let set = CapSet::root(vec![Grant::grant("host:fs/*"), Grant::grant("lifecycle")]);
        assert!(set.holds("host:fs/read"));
        assert!(set.holds("lifecycle"));
        assert!(!set.holds("host:exec"));
        assert!(set.may_grant("host:fs/write"));
        assert!(!set.may_grant("host:sql/query"));
    }

    #[test]
    fn deny_beats_grant() {
        let set = CapSet::root(vec![
            Grant::grant("host:fs/*"),
            Grant::deny("host:fs/secret/*"),
        ]);
        assert!(set.holds("host:fs/read"));
        assert!(!set.holds("host:fs/secret/key"));
    }

    #[test]
    fn attenuation_narrows_only() {
        let root = CapSet::root(vec![Grant::grant("host:fs/*")]);
        let child = root
            .attenuate(
                Principal("root-program".into()),
                vec![Grant::grant("host:fs/read/*")],
            )
            .unwrap();
        assert!(child.holds("host:fs/read/a"));
        assert!(!child.holds("host:fs/write"));
        let widened = child.attenuate(
            Principal("child".into()),
            vec![Grant::grant("host:fs/write")],
        );
        assert_eq!(
            widened.unwrap_err(),
            AttenuationError::NotHeldByParent {
                capability: "host:fs/write".into()
            }
        );
    }

    #[test]
    fn attenuation_keeps_denies() {
        let root = CapSet::root(vec![
            Grant::grant("host:fs/*"),
            Grant::deny("host:fs/secret"),
        ]);
        // Dropping the deny would widen: refused.
        let dropped = root.attenuate(Principal("p".into()), vec![Grant::grant("host:fs/*")]);
        assert_eq!(
            dropped.unwrap_err(),
            AttenuationError::DenyDropped {
                capability: "host:fs/secret".into()
            }
        );
        // Carrying it (or a wider deny) is fine.
        let kept = root
            .attenuate(
                Principal("p".into()),
                vec![Grant::grant("host:fs/*"), Grant::deny("host:fs/secret*")],
            )
            .unwrap();
        assert!(!kept.holds("host:fs/secret"));
    }

    #[test]
    fn provenance_chains_to_root() {
        let root = CapSet::root(vec![Grant::grant("host:*")]);
        let a = root
            .attenuate(Principal("root".into()), vec![Grant::grant("host:fs/*")])
            .unwrap();
        let b = a
            .attenuate(
                Principal("agent-1".into()),
                vec![Grant::grant("host:fs/read")],
            )
            .unwrap();
        let grantors: Vec<_> = b.chain().map(|(p, _)| p.0.clone()).collect();
        assert_eq!(grantors, ["agent-1", "root"]);
        assert_eq!(root.chain().count(), 0);
    }

    struct PathScope;
    impl ScopeType for PathScope {
        fn describe(&self) -> &str {
            "a directory path"
        }
        fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
            match scope {
                Some(Scope(v)) if v.is_str() => Ok(()),
                Some(_) => Err("scope is not a string".into()),
                None => Err("scope is required".into()),
            }
        }
    }

    #[test]
    fn ill_scoped_grants_fail_at_startup_by_name() {
        let mut reg = ScopeRegistry::new();
        reg.declare("host:fs/*", PathScope);
        reg.declare("host:time*", NoScope);

        let good = [
            Grant::grant("host:fs/read").with_scope("data/".into()),
            Grant::grant("host:time"),
            Grant::grant("something:undeclared"),
        ];
        assert_eq!(reg.validate(&good), Ok(()));

        let missing = [Grant::grant("host:fs/read")];
        let err = reg.validate(&missing).unwrap_err();
        assert_eq!(err.capability, "host:fs/read");
        assert!(err.to_string().contains("a directory path"));

        let extra = [Grant::grant("host:time").with_scope("nope".into())];
        assert!(reg.validate(&extra).is_err());
    }
}
