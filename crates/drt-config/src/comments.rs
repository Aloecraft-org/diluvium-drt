//! `_`-prefixed keys are comments. Everywhere.
//!
//! JSON has no comment syntax, and the reasoning that lived in a
//! `.host.lua`'s comments lives in `_`-prefixed keys now
//! (`examples/deployment.json` set the convention). A comment must not
//! reach a reader, for two reasons that pull the same way. Every config
//! struct refuses a key it does not know (issue #31: a misspelled bound was
//! a bound that silently did not apply, and `creat` for `create` was a
//! database Litestream could not replicate while `/health` answered 200),
//! so a comment left in place would be refused as a typo. And a map whose
//! keys are names, such as `relay.labels`, read a comment as a name with a
//! value of the wrong shape and refused it naming the struct rather than
//! the convention.
//!
//! One rule, no exemptions: a key beginning with [`MARKER`] is removed
//! wherever it sits -- in a struct, in a map of names, inside a connector's
//! `scope`, inside `args` -- before anything reads the document. The old
//! caveat that a connector's scope was passed through verbatim, with an
//! underscore key there being data, is gone with it; no connector ever read
//! one. A comment is a comment because of how it is spelled, not because
//! of where it sits, which is the property the file's author can see.
//!
//! Pure. Here rather than in drt because dollup reads the same documents
//! and should strip them the same way.
//!
//! ## surface block
//!
//! - Entry points: [`strip`], the whole of it; [`is_comment`], the rule for
//!   one key.
//! - Configurable values: [`MARKER`], the prefix that makes a key a comment.
//! - Fan-out: none. Objects are walked, arrays are walked, scalars are left.

use serde_json::Value;

/// The prefix that makes a key a comment.
pub const MARKER: char = '_';

/// Whether a key is a comment.
pub fn is_comment(key: &str) -> bool {
    key.starts_with(MARKER)
}

/// Remove every comment key from `value`, at every depth.
pub fn strip(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_comment(key));
            for child in map.values_mut() {
                strip(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip(item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_comment_goes_wherever_it_sits() {
        let mut v = json!({
            "_top": "why",
            "numeric": { "_why": 1, "max_elements": 64 },
            "connectors": { "_note": "a map of names", "fs": { "scope": { "_note": "inside a scope", "scope": "." } } },
            "labels": { "_note": "a map of names", "abc": { "_who": "this machine", "park_key": "k" } },
            "list": [ { "_x": 1, "y": 2 }, 3 ]
        });
        strip(&mut v);
        assert_eq!(
            v,
            json!({
                "numeric": { "max_elements": 64 },
                "connectors": { "fs": { "scope": { "scope": "." } } },
                "labels": { "abc": { "park_key": "k" } },
                "list": [ { "y": 2 }, 3 ]
            })
        );
    }

    #[test]
    fn a_key_is_a_comment_by_its_first_character_alone() {
        assert!(is_comment("_"));
        assert!(is_comment("__"));
        assert!(is_comment("_note"));
        assert!(!is_comment("note_"));
        assert!(!is_comment("a_b"));
        assert!(!is_comment(""));
    }

    #[test]
    fn scalars_and_empties_are_left_alone() {
        for mut v in [json!(1), json!("s"), json!(null), json!([]), json!({})] {
            let before = v.clone();
            strip(&mut v);
            assert_eq!(v, before);
        }
    }
}
