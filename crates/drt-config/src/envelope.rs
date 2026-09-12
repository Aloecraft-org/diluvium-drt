//! The envelope: the computed record of what a root has committed.
//!
//! `project.json` is declared and never computed; this is the other half, and
//! the only computed thing a root carries that travels. `commit` is its single
//! writer, so audit has one current envelope to check, and `push` signs the
//! envelope's hash rather than signing a file list it assembled itself.
//!
//! ## surface block
//!
//! - Entry points: [`Envelope`], the record; [`Envelope::hash`], what gets
//!   signed; [`content_hash`], one file's hash.
//! - Configurable values: [`FILENAME`], where it lives under `state/`.
//!
//! **Both hashing regimes meet here, and the boundary is the point.** A file's
//! hash is over its **bytes as they are**, which is dollup's artifact regime:
//! an artifact's identity has to survive transport byte for byte, and
//! canonicalizing a `.dlua` file would be absurd. The envelope *object*,
//! though, is authored into a type and re-serialized before anyone signs it, so
//! hashing it goes through canonical JSON like every other signed object in
//! this crate. One regime per question, and this comment is the boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::canon::{self, Hash};
use crate::id::Uuid7;
use crate::time::Timestamp;

/// Under `state/`, because the envelope is runtime-owned and `commit` writes
/// it. It does not travel as a file: `push` sends its hash.
pub const FILENAME: &str = "envelope.json";

/// What a root has committed, and the hash of each file of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// Which root this is the content of. An envelope that travelled, or one
    /// left by a different root at this path, is caught by the same comparison
    /// `consent.json` is.
    pub root_id: Uuid7,
    /// Path relative to `init/`, to the hash of its bytes. A `BTreeMap` so the
    /// order is the paths' and not an insertion order nobody chose -- the
    /// envelope is hashed, and a map that serialized in walk order would hash
    /// differently on two filesystems that list a directory differently.
    pub files: BTreeMap<String, Hash>,
    pub committed_at: Timestamp,
}

impl Envelope {
    pub fn new(root_id: Uuid7, committed_at: Timestamp) -> Envelope {
        Envelope {
            root_id,
            files: BTreeMap::new(),
            committed_at,
        }
    }

    /// The hash `push` signs and an approval names.
    ///
    /// Over the whole envelope, canonically, which means it moves when any
    /// file's content moves and when nothing else does. Fallible for the same
    /// reason [`crate::project::ceiling_hash`] is: a value that cannot be
    /// expressed as JSON is a named failure rather than a panic.
    pub fn hash(&self) -> Result<Hash, String> {
        let value = serde_json::to_value(self).map_err(|e| format!("the envelope: {e}"))?;
        Ok(canon::hash_value(&value))
    }

    /// Does this envelope describe exactly these files, with these contents?
    ///
    /// Audit's "does the envelope hash match the committed content?" in one
    /// call. It answers with *what differs* rather than a boolean: "your root
    /// does not match its envelope" is not something anyone can act on.
    pub fn differences(&self, actual: &BTreeMap<String, Hash>) -> Vec<Difference> {
        let mut out = Vec::new();
        for (path, hash) in &self.files {
            match actual.get(path) {
                None => out.push(Difference::Missing { path: path.clone() }),
                Some(found) if found != hash => {
                    out.push(Difference::Changed { path: path.clone() })
                }
                Some(_) => {}
            }
        }
        for path in actual.keys() {
            if !self.files.contains_key(path) {
                out.push(Difference::Uncommitted { path: path.clone() });
            }
        }
        out
    }
}

/// One way a root and its envelope disagree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Difference {
    #[error("'{path}' is in the envelope and not in init/")]
    Missing { path: String },
    #[error("'{path}' has changed since it was committed")]
    Changed { path: String },
    #[error("'{path}' is in init/ and not in the envelope")]
    Uncommitted { path: String },
}

/// One file's hash: its bytes, as they are.
///
/// The artifact regime, not the canonical one -- see the module header. Taking
/// bytes rather than a path because this crate opens nothing.
pub fn content_hash(bytes: &[u8]) -> Hash {
    canon::hash(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Uuid7 {
        Uuid7::parse("0192f0c1-8000-7000-8000-00000000abcd").unwrap()
    }

    fn at() -> Timestamp {
        Timestamp::parse("2026-09-12T00:00:00Z").unwrap()
    }

    fn envelope(files: &[(&str, &[u8])]) -> Envelope {
        let mut envelope = Envelope::new(id(), at());
        for (path, bytes) in files {
            envelope
                .files
                .insert((*path).to_string(), content_hash(bytes));
        }
        envelope
    }

    /// The hash moves when content moves and stands still otherwise, which is
    /// the whole job.
    #[test]
    fn the_hash_follows_the_content() {
        let before = envelope(&[("app.dlua", b"print('hi')")]).hash().unwrap();

        assert_eq!(
            envelope(&[("app.dlua", b"print('hi')")]).hash().unwrap(),
            before,
            "the same content hashes the same"
        );
        assert_ne!(
            envelope(&[("app.dlua", b"print('hi')\\n")]).hash().unwrap(),
            before,
            "one byte moves it"
        );
        assert_ne!(
            envelope(&[("app.dlua", b"print('hi')"), ("lib.dlua", b"")])
                .hash()
                .unwrap(),
            before,
            "and so does a new file"
        );
    }

    /// Insertion order must not reach the hash: two filesystems listing a
    /// directory differently would otherwise produce two envelopes for one
    /// tree.
    #[test]
    fn the_hash_does_not_depend_on_the_order_files_were_walked() {
        let mut forwards = Envelope::new(id(), at());
        forwards.files.insert("a.dlua".into(), content_hash(b"a"));
        forwards.files.insert("b.dlua".into(), content_hash(b"b"));

        let mut backwards = Envelope::new(id(), at());
        backwards.files.insert("b.dlua".into(), content_hash(b"b"));
        backwards.files.insert("a.dlua".into(), content_hash(b"a"));

        assert_eq!(forwards.hash().unwrap(), backwards.hash().unwrap());
    }

    /// Differences name what differs, because "does not match" is not
    /// actionable.
    #[test]
    fn differences_name_each_file_and_how_it_differs() {
        let committed = envelope(&[("app.dlua", b"one"), ("gone.dlua", b"two")]);
        let actual = BTreeMap::from([
            ("app.dlua".to_string(), content_hash(b"edited")),
            ("new.dlua".to_string(), content_hash(b"three")),
        ]);

        let differences = committed.differences(&actual);
        assert!(differences.contains(&Difference::Changed {
            path: "app.dlua".into()
        }));
        assert!(differences.contains(&Difference::Missing {
            path: "gone.dlua".into()
        }));
        assert!(differences.contains(&Difference::Uncommitted {
            path: "new.dlua".into()
        }));
        assert_eq!(differences.len(), 3);

        // And an agreeing pair differs in nothing.
        let same: BTreeMap<String, Hash> = committed.files.clone();
        assert!(committed.differences(&same).is_empty());
    }

    #[test]
    fn round_trips_through_json_and_refuses_an_unknown_field() {
        let envelope = envelope(&[("app.dlua", b"print('hi')")]);
        let text = serde_json::to_string(&envelope).unwrap();
        assert_eq!(serde_json::from_str::<Envelope>(&text).unwrap(), envelope);

        let e = serde_json::from_str::<Envelope>(
            r#"{"root_id":"0192f0c1-8000-7000-8000-00000000abcd","files":{},
                "committed_at":"2026-09-12T00:00:00Z","signed_by":"someone"}"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("signed_by"), "{e}");
    }
}
