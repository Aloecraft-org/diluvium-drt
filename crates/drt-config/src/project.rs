//! `project.json`: the root descriptor, and the declared ceiling.
//!
//! Everything here is **declared, never computed**. `root_id` is minted
//! once and random, `drt` is written by `dollup pin`, `caps` is hand-written
//! by whoever owns the root. Nothing in this file depends on what the code
//! says, which is the property that makes editing a profile unable to
//! invalidate it. Content hashes live in the envelope, written by `commit`.
//!
//! ## surface block
//!
//! - Entry points: [`ProjectJson`], the file as a type; [`DeclaredCeiling`],
//!   the two halves an operator consents to; [`ceiling_hash`], the hash
//!   consent.md §4 compares; [`ProfileName`], the filename-to-name rule;
//!   [`NodePath`], a node's derived identity; [`QualifiedNode`], that path
//!   with its root attached for addressing a peer; [`PeerDeclaration`], one
//!   expected peer; [`is_reserved`], the name refusal both binaries run.
//! - Configurable values: [`RESERVED`], the reserved names;
//!   [`PROFILE_SUFFIX`], the profile filename suffix; [`FALLBACK_ORDER`],
//!   the pre-recognized configs consulted only when there is no
//!   `project.json`; [`ROOT_DIR`] and the names inside it.
//! - Fan-out: [`AllowNested`] is the nesting policy's three answers.
//!
//! **`deny_unknown_fields`, deliberately.** A newer dollup writing a field
//! this drt does not know fails by name instead of being silently ignored,
//! and for `caps` specifically that is the difference between an operator's
//! consent meaning what they read and a future ceiling-shaping field being
//! dropped on the floor — under-enforcing a ceiling somebody accepted. The
//! repository settled this shape once already, in the `.host.lua` loader:
//! "a typo about to become a silent default is the failure mode this loader
//! exists to catch." The cost is real and it is dollup's: it must not write
//! a field before the pinned drt knows it, which is what the pin is for.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use drt_caps::Grant;

use crate::canon::{self, Hash};
use crate::id::Uuid7;

/// The directory that claims a path as a root.
pub const ROOT_DIR: &str = ".drt_root";
/// Declared content: what `pull` writes and what `commit` captures.
pub const INIT_DIR: &str = "init";
/// What runs. `deploy` writes it; `rm` removes it.
pub const LIVE_DIR: &str = "live";
/// Per-node stdout, unless a profile redirects it.
pub const LOG_DIR: &str = "log";
/// The profiles, of which [`ProjectJson::profiles`] is the authority.
pub const PROFILE_DIR: &str = "profile";
/// Runtime-owned, and it never travels: the envelope, the active profile,
/// and the GSR directories. `consent.json` is deliberately **not** here —
/// operator-owned versus runtime-owned is the line this directory draws.
pub const STATE_DIR: &str = "state";

/// Names no node, profile, package or project may take, case-folded.
///
/// Refused on both sides: drt at spawn and at profile resolution, dollup in
/// its admission path (add, repo seal, index scan). One list, so a name
/// accepted by one and refused by the other cannot exist.
pub const RESERVED: &[&str] = &["drt", INIT_DIR, LIVE_DIR, LOG_DIR, PROFILE_DIR, STATE_DIR];

/// Every profile filename is `<name>.config.json`; the profile's name is
/// `<name>`. A bare `config.json` is therefore not a valid profile
/// filename, which is why the fallback order below spells the middle one
/// `default.config.json`.
pub const PROFILE_SUFFIX: &str = ".config.json";

/// The pre-recognized configs, in the order `start` tries them, used
/// **only** when there is no `project.json`. With one present,
/// `default_profile` and `profiles` decide and this list is not consulted.
pub const FALLBACK_ORDER: &[&str] = &[
    "debug.config.json",
    "default.config.json",
    "release.config.json",
];

/// Is this name reserved? Case-folded, because `Live` and `live` naming the
/// same directory on a case-insensitive filesystem is exactly the collision
/// the list exists to prevent.
pub fn is_reserved(name: &str) -> bool {
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name))
}

/// What to do when this root's path lies under another root's.
///
/// The **inner** root governs, because the outer root's config cannot be
/// relied on to be readable — a root nested inside someone else's tree is
/// the case this exists for, and asking the outer one for permission would
/// mean reading a file you may not have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllowNested {
    Allow,
    #[default]
    Warn,
    Error,
}

/// A source dollup resolves packages from.
///
/// Opaque to drt, which never interprets one: the element shape belongs to
/// `dollup-format` and this alias is the seam where it will be replaced by
/// the real type when dollup takes this dependency. Typed as a value rather
/// than skipped so that `deny_unknown_fields` does not reject a field drt
/// has no business reading.
pub type Source = serde_json::Value;

/// `.drt_root/project.json`.
///
/// Deliberately **not** `Default`: every other field has a sane absent form,
/// and `root_id` does not. A defaulted `root_id` would be a value two roots
/// could share, and `root_id` is the hinge shipping turns on — a pull landing
/// on a matching one is a restore. [`ProjectJson::new`] is the only way to
/// build one, and it takes the minted id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectJson {
    /// Absent is legal and surfaced by audit rather than refused: `dollup
    /// init` with no argument is a real way to start, and naming the project
    /// later is cheap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_version: Option<String>,
    /// The hinge shipping turns on. Minted once when `.drt_root/` is
    /// created, random and never derived, so two roots cannot collide by
    /// having the same content.
    pub root_id: Uuid7,
    /// Set by `dollup duplicate`: this root was copied from that one and
    /// given a fresh `root_id`. Absent on a root that was not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicated_from: Option<Uuid7>,
    /// The pinned runtime version. Per root rather than per profile because
    /// there is one binary at `.drt_root/drt` and a per-profile pin could
    /// not be satisfied. A pin disagreeing with the binary present is a
    /// named failure at start that names both versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drt: Option<String>,
    /// **The ceiling**, declared here and nowhere else. Every listed
    /// profile's `caps` must attenuate under it, and consent.md hashes this
    /// subtree. Empty in a root is a named failure at start, not a wide
    /// grant: the wide default belongs to the no-root path alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<Grant>,
    /// **The other half of the ceiling**: the peers this root expects, by
    /// role. Declared here, bound to actual peers in `consent.json`, and
    /// hashed with `caps` as the one thing consent.md §4 protects — so
    /// declaring an inbound peer prompts, exactly as adding a cap does.
    ///
    /// Part of the ceiling rather than beside it because an inbound peer is
    /// reach into this root that the operator has not otherwise agreed to.
    /// A ceiling hashed over `caps` alone would let a new role arrive
    /// silently, which is the hole this field closes.
    ///
    /// Empty is the ordinary case and is not a failure: a root that talks to
    /// nobody declares nothing. That is why it does not share `caps`'s
    /// "empty is a named failure" rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peers: Vec<PeerDeclaration>,
    #[serde(default, skip_serializing_if = "is_default_nesting")]
    pub allow_nested: AllowNested,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source>,
    /// dollup's to enforce; drt never reads it. Defaulted **true** so an
    /// older descriptor without the field fails closed.
    #[serde(default = "default_true")]
    pub require_signatures: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// Authoritative. A config present in `profile/` but absent from this
    /// list is ignored, which is what stops a stray `debug.config.json`
    /// taking effect on a deployed root. Holds filenames, not names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
}

/// One expected peer, by the role this root calls it.
///
/// **Declared, never computed**, like everything else in this file. A root
/// ships the peers it expects; the operator binds each role to an actual
/// peer on this box in `consent.json`, which never travels. That split is
/// what lets the same root run against a different database on two boxes
/// without editing the root.
///
/// A peer is either another drt root or a plugin, and this side of the
/// declaration cannot tell which — nor should it. Which kind satisfies the
/// role is the operator's binding, so `role`, `queues` and `contract` are
/// the same three fields either way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerDeclaration {
    /// What this root calls the peer. Local to this root: two roots may use
    /// the same role name for different peers, and the binding decides.
    pub role: String,
    /// The queues this root expects to write to or read from on that peer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues: Vec<String>,
    /// A version string both sides declare. Compatibility is **string
    /// equality** and nothing else.
    ///
    /// That is a flag day on purpose: `discofetch-db/1` to `/2` has no
    /// overlap window, and every root naming it changes in one step. Fine
    /// for a few roots on one box. It is not semver, must not be parsed as
    /// one, and nothing here orders two contract strings.
    pub contract: String,
}

impl PeerDeclaration {
    /// A role name that can be printed in a refusal and matched against a
    /// binding: non-empty, one segment, and not a reserved name.
    ///
    /// Checked rather than assumed because a role reaches an operator's
    /// `consent.json` as a key they have to match by eye.
    pub fn check_role(role: &str) -> Result<(), BadRole> {
        if role.is_empty() {
            return Err(BadRole::Empty);
        }
        if role.contains('/') || role.contains('\\') {
            return Err(BadRole::Separator {
                role: role.to_string(),
            });
        }
        if is_reserved(role) {
            return Err(BadRole::Reserved {
                role: role.to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadRole {
    #[error("a peer role cannot be empty")]
    Empty,
    #[error("peer role '{role}' contains a path separator; a role is one name")]
    Separator { role: String },
    #[error("'{role}' is a reserved name and cannot name a peer role")]
    Reserved { role: String },
}

fn default_true() -> bool {
    true
}

fn is_default_nesting(value: &AllowNested) -> bool {
    *value == AllowNested::default()
}

/// The declared ceiling: what an operator consents to, and the only thing
/// consent.md §4 hashes.
///
/// Two halves, because a ceiling is two questions. `caps` is what this root
/// may reach out and do. `peers` is who may be named on the other end of a
/// queue write, which is reach both ways and so equally the operator's to
/// agree to. Hashing `caps` alone would let a new inbound peer arrive
/// without a prompt.
///
/// **Both keys are always present in the preimage**, empty or not. An
/// omitted-when-empty `peers` would give one ceiling two hashes depending
/// on which code path built it, and two implementers hashing different
/// bytes for the same ceiling is the failure this shape exists to prevent.
/// The cost is one silent re-write per existing root, the first time it
/// starts under a drt that knows this field: identical caps and no peers
/// is a no-op edit, which lands on the silent narrowing path and rewrites
/// the entry in the new shape.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct DeclaredCeiling {
    pub caps: Vec<Grant>,
    pub peers: Vec<PeerDeclaration>,
}

impl DeclaredCeiling {
    pub fn new(caps: Vec<Grant>, peers: Vec<PeerDeclaration>) -> DeclaredCeiling {
        DeclaredCeiling { caps, peers }
    }

    /// A ceiling of caps alone: what every root declared before `peers`
    /// existed, and what a caller building one in memory usually means.
    pub fn of_caps(caps: Vec<Grant>) -> DeclaredCeiling {
        DeclaredCeiling {
            caps,
            peers: Vec::new(),
        }
    }

    /// The roles this ceiling names, for the widen relation.
    pub fn roles(&self) -> Vec<&str> {
        self.peers.iter().map(|p| p.role.as_str()).collect()
    }
}

// depth: reading a stored ceiling written before `peers` existed
//
// A `consent.json` from an older drt holds `"ceiling": [ ...grants... ]`,
// a bare array. Refusing it would make every existing root fail to start
// on an upgrade, which is a worse outcome than any this change is for, so
// both spellings deserialize and only the object is ever written. One-way,
// and the rewrite happens on the silent narrowing path the first time the
// root starts.
impl<'de> Deserialize<'de> for DeclaredCeiling {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<DeclaredCeiling, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Legacy(Vec<Grant>),
            Current {
                #[serde(default)]
                caps: Vec<Grant>,
                #[serde(default)]
                peers: Vec<PeerDeclaration>,
            },
        }
        Ok(match Either::deserialize(d)? {
            Either::Legacy(caps) => DeclaredCeiling::of_caps(caps),
            Either::Current { caps, peers } => DeclaredCeiling { caps, peers },
        })
    }
}

/// The hash consent.md §4 compares: canonical JSON over the declared
/// ceiling — `caps` and `peers` — and nothing else.
///
/// Over those two subtrees rather than the whole file precisely so that
/// bumping `project_version` or adding a profile does not invalidate
/// consent. Editing the ceiling is the one edit that should, and this is
/// why.
///
/// Fallible because a [`drt_caps::Scope`] is an `rmpv::Value` and msgpack
/// can hold shapes JSON cannot (binary, an extension, a non-string map
/// key). A ceiling read from `project.json` can hold none of them, so in
/// practice this is a named failure for a descriptor built in memory — and
/// a named failure is what consent.md asks for over a panic.
pub fn ceiling_hash(project: &ProjectJson) -> Result<Hash, CeilingHashError> {
    ceiling_hash_of(&declared_ceiling(project))
}

/// The declared ceiling this descriptor states.
pub fn declared_ceiling(project: &ProjectJson) -> DeclaredCeiling {
    DeclaredCeiling {
        caps: project.caps.clone(),
        peers: project.peers.clone(),
    }
}

/// [`ceiling_hash`] over a ceiling in hand: what consent.md's stored
/// `ceiling` is re-hashed as when an accepted entry is checked.
pub fn ceiling_hash_of(ceiling: &DeclaredCeiling) -> Result<Hash, CeilingHashError> {
    let value = serde_json::to_value(ceiling).map_err(|e| CeilingHashError {
        detail: e.to_string(),
    })?;
    Ok(canon::hash_value(&value))
}

/// [`ceiling_hash_of`] for a ceiling of caps and no peers.
pub fn caps_hash(caps: &[Grant]) -> Result<Hash, CeilingHashError> {
    ceiling_hash_of(&DeclaredCeiling::of_caps(caps.to_vec()))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the ceiling cannot be hashed: {detail}; a scope holding something JSON has no form for cannot be consented to")]
pub struct CeilingHashError {
    pub detail: String,
}

// depth: the filename-to-name rule, which two commands and audit all need

/// A profile's name and the filename it lives under.
///
/// Both spellings exist in one file — `profiles` holds filenames,
/// `default_profile` holds a name — so the conversion is written once here
/// rather than at each of the three call sites that would otherwise each
/// get it slightly wrong.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProfileName(String);

impl ProfileName {
    /// A name (`debug`) to a profile name, refusing a reserved one.
    pub fn new(name: &str) -> Result<ProfileName, BadProfileName> {
        if name.is_empty() {
            return Err(BadProfileName::Empty);
        }
        if is_reserved(name) {
            return Err(BadProfileName::Reserved {
                name: name.to_string(),
            });
        }
        if name.contains('.') || name.contains('/') || name.contains('\\') {
            return Err(BadProfileName::NotAName {
                name: name.to_string(),
            });
        }
        Ok(ProfileName(name.to_string()))
    }

    /// A filename (`debug.config.json`) to a profile name.
    pub fn from_filename(filename: &str) -> Result<ProfileName, BadProfileName> {
        let Some(stem) = filename.strip_suffix(PROFILE_SUFFIX) else {
            return Err(BadProfileName::NotAProfileFilename {
                filename: filename.to_string(),
            });
        };
        ProfileName::new(stem)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The filename this name lives under.
    pub fn filename(&self) -> String {
        format!("{}{PROFILE_SUFFIX}", self.0)
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadProfileName {
    #[error("a profile name cannot be empty")]
    Empty,
    #[error("'{name}' is a reserved name and cannot name a profile")]
    Reserved { name: String },
    #[error("'{name}' is not a profile name; a name has no '.', '/' or '\\' in it -- '{name}{PROFILE_SUFFIX}' is the filename")]
    NotAName { name: String },
    #[error(
        "'{filename}' is not a profile filename; every profile is named '<name>{PROFILE_SUFFIX}'"
    )]
    NotAProfileFilename { filename: String },
}

impl ProjectJson {
    /// A descriptor for a freshly minted root. Every other field takes its
    /// absent form; `dollup init` fills them in.
    pub fn new(root_id: Uuid7) -> ProjectJson {
        ProjectJson {
            project_name: None,
            project_version: None,
            root_id,
            duplicated_from: None,
            drt: None,
            caps: Vec::new(),
            peers: Vec::new(),
            allow_nested: AllowNested::default(),
            sources: Vec::new(),
            require_signatures: true,
            default_profile: None,
            profiles: Vec::new(),
        }
    }

    /// The declared profiles, by name, with the filename each came from.
    ///
    /// A filename in `profiles` that is not a profile filename is reported
    /// rather than skipped: silently ignoring it would make `profiles`
    /// authoritative over a list nobody can see is wrong.
    pub fn declared_profiles(&self) -> (BTreeMap<ProfileName, String>, Vec<BadProfileName>) {
        let mut named = BTreeMap::new();
        let mut bad = Vec::new();
        for filename in &self.profiles {
            match ProfileName::from_filename(filename) {
                Ok(name) => {
                    named.insert(name, filename.clone());
                }
                Err(e) => bad.push(e),
            }
        }
        (named, bad)
    }
}

// depth: node paths, which is what the reserved-name rule bites on

/// A node's place in the tree, `/`-separated from the root node: `root`,
/// `root/intake`, `root/intake/worker`.
///
/// **Derived at spawn from the parent's path, never declared.** consent.md
/// §6 puts this in a GSR request, and a request whose subject is a string
/// the asking node chose is not an audit record. Nothing composes one today
/// — an instance is an `InstanceId(u32)` plus a `LoadSpec` name — so this
/// type is the shape that composition produces.
///
/// It is also what gives "`drt` is a privileged name" something to bite on:
/// every segment goes through [`is_reserved`], so `root/drt` does not exist
/// and a node cannot name itself after the runtime, the state directory, or
/// any other name that means something at a root's top level.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodePath(String);

/// What the root node is called. One per root, and the ceiling is its
/// capability set.
pub const ROOT_NODE: &str = "root";

impl NodePath {
    /// The root node.
    pub fn root() -> NodePath {
        NodePath(ROOT_NODE.to_string())
    }

    /// A child of this node. The only way a deeper path is built, which is
    /// what keeps "derived, not declared" true by construction.
    pub fn child(&self, name: &str) -> Result<NodePath, BadNodePath> {
        check_segment(name)?;
        Ok(NodePath(format!("{}/{name}", self.0)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// This node's parent, or `None` for the root node.
    pub fn parent(&self) -> Option<NodePath> {
        self.0
            .rsplit_once('/')
            .map(|(head, _)| NodePath(head.to_string()))
    }

    /// Text to a path, for reading a GSR request back off disk. Every
    /// segment is checked, including the first, which must be [`ROOT_NODE`]:
    /// a path that does not start at the root node is not a path in this
    /// root.
    pub fn parse(text: &str) -> Result<NodePath, BadNodePath> {
        let mut segments = text.split('/');
        match segments.next() {
            Some(ROOT_NODE) => {}
            _ => {
                return Err(BadNodePath::NotRooted {
                    path: text.to_string(),
                })
            }
        }
        for segment in segments {
            check_segment(segment)?;
        }
        Ok(NodePath(text.to_string()))
    }

    /// This node with its root attached, for addressing a peer.
    pub fn qualified(&self, root_id: Uuid7) -> QualifiedNode {
        QualifiedNode {
            root_id,
            node: self.clone(),
        }
    }
}

// depth: the qualified form, and why it is a separate type
//
// `<root_id>/root/intake` names a node on a peer. It exists for addressing
// and for nothing else: the moment it reaches a hash it would carry the
// root twice, once in this string and once in the object's own `root_id`
// field, and two implementers would disagree about which. So the qualified
// form is a distinct type that no preimage accepts, and `NodePath` — the
// local form — stays the only thing a hashed object can hold.
//
// The two spellings cannot be confused: a local path's first segment is
// always `root`, and a uuid7 never is.

/// A node path with the root it lives in: `<root_id>/root/intake`.
///
/// **Addressing only.** Hashed objects (a GSR identity, consent.md §6)
/// carry [`NodePath`] — the local form — and `root_id` once as its own
/// field. Nothing here goes into a preimage.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QualifiedNode {
    root_id: Uuid7,
    node: NodePath,
}

impl QualifiedNode {
    pub fn new(root_id: Uuid7, node: NodePath) -> QualifiedNode {
        QualifiedNode { root_id, node }
    }

    pub fn root_id(&self) -> Uuid7 {
        self.root_id
    }

    /// The local form: the suffix after the first `/`, and what every
    /// hashed object carries.
    pub fn node(&self) -> &NodePath {
        &self.node
    }

    pub fn into_parts(self) -> (Uuid7, NodePath) {
        (self.root_id, self.node)
    }

    /// Text to a qualified path. The head is the root id, the suffix after
    /// the first `/` is an ordinary node path and is checked as one.
    pub fn parse(text: &str) -> Result<QualifiedNode, BadQualifiedNode> {
        let Some((head, rest)) = text.split_once('/') else {
            return Err(BadQualifiedNode::NotQualified {
                path: text.to_string(),
            });
        };
        let root_id = Uuid7::parse(head).map_err(|e| BadQualifiedNode::Root {
            head: head.to_string(),
            detail: e.to_string(),
        })?;
        let node = NodePath::parse(rest)?;
        Ok(QualifiedNode { root_id, node })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadQualifiedNode {
    #[error("'{path}' names no root; a qualified node path is '<root_id>/{ROOT_NODE}/...'")]
    NotQualified { path: String },
    #[error("'{head}' does not lead a qualified node path: {detail}")]
    Root { head: String, detail: String },
    #[error(transparent)]
    Node(#[from] BadNodePath),
}

impl std::fmt::Display for QualifiedNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.root_id, self.node)
    }
}

impl From<QualifiedNode> for String {
    fn from(q: QualifiedNode) -> String {
        q.to_string()
    }
}

impl TryFrom<String> for QualifiedNode {
    type Error = BadQualifiedNode;

    fn try_from(text: String) -> Result<QualifiedNode, BadQualifiedNode> {
        QualifiedNode::parse(&text)
    }
}

fn check_segment(name: &str) -> Result<(), BadNodePath> {
    if name.is_empty() {
        return Err(BadNodePath::EmptySegment);
    }
    if is_reserved(name) {
        return Err(BadNodePath::Reserved {
            name: name.to_string(),
        });
    }
    if name.contains('/') || name.contains('\\') {
        return Err(BadNodePath::Separator {
            name: name.to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadNodePath {
    #[error("'{path}' does not start at '{ROOT_NODE}'; every node path is rooted there")]
    NotRooted { path: String },
    #[error("a node name cannot be empty")]
    EmptySegment,
    #[error("'{name}' is a reserved name and cannot name a node")]
    Reserved { name: String },
    #[error("'{name}' contains a path separator; a node name is one segment")]
    Separator { name: String },
}

impl std::fmt::Display for NodePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<NodePath> for String {
    fn from(p: NodePath) -> String {
        p.0
    }
}

impl TryFrom<String> for NodePath {
    type Error = BadNodePath;
    fn try_from(s: String) -> Result<NodePath, BadNodePath> {
        NodePath::parse(&s)
    }
}

#[cfg(test)]
mod node_path_tests {
    use super::*;

    #[test]
    fn a_path_is_built_by_descending_from_the_root() {
        let root = NodePath::root();
        let intake = root.child("intake").unwrap();
        let worker = intake.child("worker").unwrap();

        assert_eq!(worker.as_str(), "root/intake/worker");
        assert_eq!(worker.parent().unwrap(), intake);
        assert_eq!(root.parent(), None);
        assert_eq!(
            worker.segments().collect::<Vec<_>>(),
            ["root", "intake", "worker"]
        );
    }

    /// What "drt is a privileged name" means in code.
    #[test]
    fn a_node_cannot_be_named_after_the_runtime_or_a_root_directory() {
        let root = NodePath::root();
        for name in ["drt", "DRT", "state", "live", "init", "log", "profile"] {
            assert!(
                matches!(root.child(name), Err(BadNodePath::Reserved { .. })),
                "{name}"
            );
            assert!(NodePath::parse(&format!("root/{name}")).is_err(), "{name}");
        }
        assert!(root.child("intake").is_ok());
    }

    #[test]
    fn a_path_is_parsed_not_trusted() {
        assert_eq!(
            NodePath::parse("root/intake").unwrap(),
            NodePath::root().child("intake").unwrap()
        );
        assert!(matches!(
            NodePath::parse("intake/worker"),
            Err(BadNodePath::NotRooted { .. })
        ));
        assert!(matches!(
            NodePath::parse("root//worker"),
            Err(BadNodePath::EmptySegment)
        ));
    }

    #[test]
    fn round_trips_through_json() {
        let path = NodePath::parse("root/intake").unwrap();
        let text = serde_json::to_string(&path).unwrap();
        assert_eq!(text, "\"root/intake\"");
        assert_eq!(serde_json::from_str::<NodePath>(&text).unwrap(), path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectJson {
        ProjectJson {
            project_name: Some("my_drt_project".into()),
            project_version: Some("0.0.0".into()),
            root_id: Uuid7::mint(1_757_707_440_000, [0x11; 10]),
            drt: Some("0.5.0".into()),
            caps: vec![Grant::grant("host:fs/*"), Grant::grant("host:time")],
            default_profile: Some("debug".into()),
            profiles: vec!["debug.config.json".into(), "preflight.config.json".into()],
            ..ProjectJson::new(Uuid7::mint(1_757_707_440_000, [0x11; 10]))
        }
    }

    /// The property the whole "declared, never computed" rule exists for.
    #[test]
    fn editing_anything_but_caps_leaves_the_ceiling_hash_alone() {
        let before = ceiling_hash(&project()).unwrap();

        let mut edited = project();
        edited.project_version = Some("9.9.9".into());
        edited.profiles.push("release.config.json".into());
        edited.default_profile = Some("release".into());
        assert_eq!(ceiling_hash(&edited).unwrap(), before);

        let mut widened = project();
        widened.caps.push(Grant::grant("host:exec/run"));
        assert_ne!(
            ceiling_hash(&widened).unwrap(),
            before,
            "editing the ceiling is the one edit that invalidates consent"
        );
    }

    /// An unknown field is a named failure, not a silent default. The
    /// under-enforcement case is the one that matters: a future field that
    /// shapes the ceiling must not be dropped.
    #[test]
    fn an_unknown_field_is_refused_by_name() {
        let json = r#"{"root_id":"0192f0c1-8000-7000-8000-00000000abcd","ceiling_mode":"strict"}"#;
        let e = serde_json::from_str::<ProjectJson>(json).unwrap_err();
        assert!(e.to_string().contains("ceiling_mode"), "{e}");
    }

    /// serde already refuses a repeated struct field, which is the
    /// duplicate-key trap closed for every authored file in this crate.
    #[test]
    fn a_duplicated_field_is_refused_by_name() {
        let json = r#"{"root_id":"0192f0c1-8000-7000-8000-00000000abcd","caps":[],"caps":[]}"#;
        let e = serde_json::from_str::<ProjectJson>(json).unwrap_err();
        assert!(e.to_string().contains("caps"), "{e}");
    }

    #[test]
    fn require_signatures_defaults_closed() {
        let json = r#"{"root_id":"0192f0c1-8000-7000-8000-00000000abcd"}"#;
        let p: ProjectJson = serde_json::from_str(json).unwrap();
        assert!(p.require_signatures, "absent means required, not optional");
        assert_eq!(p.allow_nested, AllowNested::Warn);
    }

    #[test]
    fn a_profile_filename_and_its_name_convert_both_ways() {
        let name = ProfileName::from_filename("debug.config.json").unwrap();
        assert_eq!(name.as_str(), "debug");
        assert_eq!(name.filename(), "debug.config.json");

        // The rule that made the fallback order spell it `default.config.json`.
        assert!(matches!(
            ProfileName::from_filename("config.json"),
            Err(BadProfileName::NotAProfileFilename { .. })
        ));
        for filename in FALLBACK_ORDER {
            assert!(
                ProfileName::from_filename(filename).is_ok(),
                "every fallback filename is a valid profile filename: {filename}"
            );
        }
    }

    #[test]
    fn reserved_names_are_refused_case_folded() {
        for name in ["drt", "DRT", "State", "live", "Init"] {
            assert!(is_reserved(name), "{name}");
            assert!(matches!(
                ProfileName::new(name),
                Err(BadProfileName::Reserved { .. })
            ));
        }
        assert!(!is_reserved("debug"));
        assert!(ProfileName::new("debug").is_ok());
    }

    #[test]
    fn declared_profiles_report_a_bad_filename_rather_than_skipping_it() {
        let mut p = project();
        p.profiles.push("config.json".into());
        let (named, bad) = p.declared_profiles();
        assert_eq!(named.len(), 2);
        assert_eq!(bad.len(), 1, "the unusable entry is reported");
    }
}

#[cfg(test)]
mod qualified_tests {
    use super::*;

    fn root_id() -> Uuid7 {
        Uuid7::mint(1_757_707_440_000, [0x22; 10])
    }

    /// Acceptance 5, first half.
    #[test]
    fn a_qualified_node_path_round_trips_and_its_suffix_is_the_local_form() {
        let local = NodePath::root().child("intake").unwrap();
        let qualified = local.qualified(root_id());

        let text = qualified.to_string();
        assert_eq!(text, format!("{}/root/intake", root_id()));
        assert_eq!(QualifiedNode::parse(&text).unwrap(), qualified);

        let (id, node) = qualified.into_parts();
        assert_eq!(id, root_id());
        assert_eq!(node, local);
        assert_eq!(
            text.split_once('/').unwrap().1,
            local.as_str(),
            "the local form is the suffix after the first '/'"
        );
    }

    /// Both forms parse, and neither is mistaken for the other. A local
    /// path always leads with `root`; a uuid7 never does.
    #[test]
    fn the_two_forms_are_told_apart_by_their_first_segment() {
        let local = "root/intake";
        assert!(NodePath::parse(local).is_ok());
        assert!(QualifiedNode::parse(local).is_err());

        let qualified = format!("{}/root/intake", root_id());
        assert!(QualifiedNode::parse(&qualified).is_ok());
        assert!(
            NodePath::parse(&qualified).is_err(),
            "a qualified path is not a local one, so it cannot reach a preimage by accident"
        );
    }

    #[test]
    fn a_qualified_path_is_refused_by_name_at_each_half() {
        let bad_root = QualifiedNode::parse("not-a-uuid/root/intake").unwrap_err();
        assert!(
            matches!(bad_root, BadQualifiedNode::Root { .. }),
            "{bad_root}"
        );

        let bad_node = QualifiedNode::parse(&format!("{}/intake", root_id())).unwrap_err();
        assert!(matches!(bad_node, BadQualifiedNode::Node(_)), "{bad_node}");

        let unqualified = QualifiedNode::parse("intake").unwrap_err();
        assert!(
            matches!(unqualified, BadQualifiedNode::NotQualified { .. }),
            "{unqualified}"
        );
    }

    #[test]
    fn a_qualified_path_serializes_as_the_one_string_it_prints() {
        let q = NodePath::root().child("db").unwrap().qualified(root_id());
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(json, format!("\"{}/root/db\"", root_id()));
        assert_eq!(serde_json::from_str::<QualifiedNode>(&json).unwrap(), q);
    }

    /// A role reaches an operator's `consent.json` as a key they match by
    /// eye, so it is checked like any other name.
    #[test]
    fn a_peer_role_is_one_unreserved_name() {
        assert!(PeerDeclaration::check_role("db").is_ok());
        assert!(matches!(
            PeerDeclaration::check_role(""),
            Err(BadRole::Empty)
        ));
        assert!(matches!(
            PeerDeclaration::check_role("a/b"),
            Err(BadRole::Separator { .. })
        ));
        assert!(matches!(
            PeerDeclaration::check_role(STATE_DIR),
            Err(BadRole::Reserved { .. })
        ));
    }

    /// The ceiling is two halves, and the hash moves when either does.
    #[test]
    fn the_hashed_ceiling_covers_both_halves() {
        let caps = vec![Grant::grant("host:fs/*")];
        let bare = ProjectJson {
            caps: caps.clone(),
            ..ProjectJson::new(root_id())
        };
        let with_peer = ProjectJson {
            caps,
            peers: vec![PeerDeclaration {
                role: "db".into(),
                queues: vec!["query".into()],
                contract: "discofetch-db/1".into(),
            }],
            ..ProjectJson::new(root_id())
        };
        assert_ne!(
            ceiling_hash(&bare).unwrap(),
            ceiling_hash(&with_peer).unwrap()
        );
    }

    /// Both keys are always in the preimage, so one ceiling has one hash
    /// however the value was built.
    #[test]
    fn an_empty_peers_list_is_still_in_the_preimage() {
        let caps = vec![Grant::grant("host:fs/*")];
        let declared = DeclaredCeiling::of_caps(caps.clone());
        let as_json = serde_json::to_value(&declared).unwrap();
        assert!(
            as_json.get("peers").is_some(),
            "an omitted-when-empty peers would give one ceiling two hashes: {as_json}"
        );
        assert_ne!(
            ceiling_hash_of(&declared).unwrap(),
            canon::hash_value(&serde_json::to_value(&caps).unwrap()),
            "the preimage is the object, not the bare cap array it used to be"
        );
    }
}
