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
//! - Entry points: [`ProjectJson`], the file as a type; [`ceiling_hash`],
//!   the hash consent.md §4 compares; [`ProfileName`], the filename-to-name
//!   rule; [`NodePath`], a node's derived identity; [`is_reserved`], the
//!   name refusal both binaries run.
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

fn default_true() -> bool {
    true
}

fn is_default_nesting(value: &AllowNested) -> bool {
    *value == AllowNested::default()
}

/// The hash consent.md §4 compares: canonical JSON over the `caps` subtree
/// and nothing else.
///
/// Over `caps` rather than the whole file precisely so that bumping
/// `project_version` or adding a profile does not invalidate consent.
/// Editing the ceiling is the one edit that should, and this is why.
///
/// Fallible because a [`drt_caps::Scope`] is an `rmpv::Value` and msgpack
/// can hold shapes JSON cannot (binary, an extension, a non-string map
/// key). A ceiling read from `project.json` can hold none of them, so in
/// practice this is a named failure for a descriptor built in memory — and
/// a named failure is what consent.md asks for over a panic.
pub fn ceiling_hash(project: &ProjectJson) -> Result<Hash, CeilingHashError> {
    caps_hash(&project.caps)
}

/// [`ceiling_hash`] over a bare cap list: what consent.md's stored
/// `ceiling` is re-hashed as when an accepted entry is checked.
pub fn caps_hash(caps: &[Grant]) -> Result<Hash, CeilingHashError> {
    let value = serde_json::to_value(caps).map_err(|e| CeilingHashError {
        detail: e.to_string(),
    })?;
    Ok(canon::hash_value(&value))
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
