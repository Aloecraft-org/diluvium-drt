//! Canonical JSON, and the one hash type every signed object names itself
//! with (consent.md §9).
//!
//! There are two hashing regimes in this system and they must not be
//! confused. **Artifacts** (a fetched manifest, a file set) hash
//! bytes-as-fetched, because an artifact's identity has to survive
//! transport byte-for-byte; that regime lives in `dollup-format` and is
//! not this. **Consent and GSR objects** hash canonically, because they
//! are authored, deserialized into types, and re-serialized before anyone
//! hashes them — the bytes on disk are not the bytes that were signed, so
//! the bytes that were signed have to be derivable from the value. Every
//! boundary between the two carries a comment saying which side it is on.
//!
//! ## surface block
//!
//! - Entry points: [`to_canonical_bytes`], a value to the bytes that get
//!   hashed and signed; [`hash`], those bytes to a [`Hash`];
//!   [`hash_value`], the two composed, which is what callers actually
//!   want; [`from_msgpack`], a guest-supplied msgpack value to a JSON
//!   value or a named refusal.
//! - Configurable: nothing. The algorithm name in [`HASH_PREFIX`] is part
//!   of the wire format, not a knob.
//! - Fan-out: [`write_canonical`] is the whole recursion, one arm per
//!   JSON kind; [`from_msgpack`]'s match is the same shape over msgpack's
//!   kinds, and [`NotCanonical`] names every way that one can refuse.
//!
//! **Why this file does not use `serde_json::to_string`.** `serde_json`'s
//! `preserve_order` feature swaps `Map`'s backing store from `BTreeMap` to
//! `IndexMap`, and cargo unifies features across a workspace. dollup's
//! workspace enables it; drt's does not. Same crate, same helper, two key
//! orders, and each repository's test suite passing in isolation — which
//! is the exact class of failure this helper exists to prevent. So sorting
//! here is *structural*: keys are collected and ordered by this file, and
//! nothing depends on how `Map` chooses to iterate. The test at the bottom
//! is what keeps that true, and it runs under both feature settings.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The algorithm, recorded in the value itself so that migrating is a
/// re-sign rather than a format break (consent.md §9).
pub const HASH_PREFIX: &str = "sha256:";

/// A content hash in its wire form, `sha256:<64 lowercase hex>`.
///
/// A newtype rather than a `String` because a `ceiling_hash` and a
/// `request_hash` are compared for equality constantly and a bare string
/// invites comparing one to something that is not a hash at all.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hash(String);

impl Hash {
    /// The wire form, prefix included.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The hex digits without the prefix — for a filename, which is the
    /// one place the `:` is inconvenient.
    pub fn hex(&self) -> &str {
        &self.0[HASH_PREFIX.len()..]
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Hash> for String {
    fn from(h: Hash) -> String {
        h.0
    }
}

impl TryFrom<String> for Hash {
    type Error = String;

    /// Parsed rather than trusted: a hash read off disk that is not a hash
    /// should fail where it was read, not at the comparison that silently
    /// never matches.
    fn try_from(s: String) -> Result<Hash, String> {
        let Some(hex) = s.strip_prefix(HASH_PREFIX) else {
            return Err(format!(
                "'{s}' is not a hash; expected '{HASH_PREFIX}<64 hex digits>'"
            ));
        };
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "'{s}' is not a hash; expected 64 lowercase hex digits after '{HASH_PREFIX}'"
            ));
        }
        if hex.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(format!("'{s}': hex digits must be lowercase, or two spellings of one hash would compare unequal"));
        }
        Ok(Hash(s))
    }
}

/// Hash already-canonical bytes. Separate from [`hash_value`] because a
/// signature covers bytes, and the signer and the verifier must be able to
/// hash *the same bytes* rather than each re-deriving them from a value.
pub fn hash(bytes: &[u8]) -> Hash {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(HASH_PREFIX.len() + 64);
    out.push_str(HASH_PREFIX);
    for byte in digest {
        use fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    Hash(out)
}

/// Canonicalize and hash: what every `ceiling_hash` and `request_hash` is.
pub fn hash_value(value: &serde_json::Value) -> Hash {
    hash(&to_canonical_bytes(value))
}

/// A value to the bytes that get hashed and signed: UTF-8, object keys
/// sorted, no insignificant whitespace.
///
/// A field excluded from a hash is *omitted* from the value handed here,
/// never blanked — `null` and absent are different bytes, and consent.md
/// §6 makes that a named failure rather than a convention.
pub fn to_canonical_bytes(value: &serde_json::Value) -> Vec<u8> {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out.into_bytes()
}

// depth: the recursion, one arm per JSON kind

/// The whole of canonicalization. Object keys are collected into a
/// `BTreeMap` and emitted in that order, so the output does not depend on
/// `serde_json::Map`'s backing store (see the module header).
///
/// Keys are ordered by their UTF-8 bytes. RFC 8785 orders by UTF-16 code
/// units, which differs only for keys containing characters above the BMP;
/// every key in this system is an ASCII field name, and the one place a
/// caller supplies keys — a guest's `ask` — is a place where the simpler
/// rule is the one worth having documented. Stated rather than discovered.
fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        // Numbers come out as `serde_json` writes them: integers exactly,
        // floats through ryu's shortest round-trip form. Deterministic for
        // a pinned serde_json, which is what the lockfile is for, and
        // non-finite floats never reach here -- `from_msgpack` refuses
        // them at the guest boundary, where the distinction still exists.
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        // Escaping is serde_json's, because hand-rolling string escaping
        // is how you end up with two spellings of one string.
        serde_json::Value::String(s) => {
            out.push_str(&serde_json::Value::String(s.clone()).to_string())
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<&str, &serde_json::Value> =
                map.iter().map(|(k, v)| (k.as_str(), v)).collect();
            out.push('{');
            for (i, (key, item)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String((*key).to_string()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
    }
}

// depth: the guest boundary, where msgpack's extra kinds are refused by name

/// Why a guest-supplied value cannot be canonicalized.
///
/// Each of these is a named failure that stops the operation, per
/// consent.md's scope note. They exist because the refusals *cannot* be
/// made after conversion: `serde_json::to_value(f64::NAN)` is
/// `Ok(Value::Null)`, silently, so a NaN that crosses into JSON is
/// indistinguishable from an author's `null`. msgpack still knows the
/// difference, so this is the last place the question can be asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotCanonical {
    #[error("{at}: {value} is not a finite number; a hashed value must round-trip, and JSON has no spelling for it")]
    NonFinite { at: String, value: String },
    #[error(
        "{at}: duplicate key '{key}'; the last one would silently win and change what was hashed"
    )]
    DuplicateKey { at: String, key: String },
    #[error("{at}: a map key must be a string, not {kind}")]
    NonStringKey { at: String, kind: &'static str },
    #[error("{at}: {kind} has no JSON form")]
    NoJsonForm { at: String, kind: &'static str },
}

/// A guest's msgpack value to a JSON value, or a named refusal.
///
/// This is consent.md §9's "explicit walk". It is the only way an `ask`
/// becomes hashable, and it runs before the value is stored, so a request
/// that could not be verified later is refused while the node is still
/// there to be told.
pub fn from_msgpack(value: &rmpv::Value) -> Result<serde_json::Value, NotCanonical> {
    walk(value, "ask")
}

fn walk(value: &rmpv::Value, at: &str) -> Result<serde_json::Value, NotCanonical> {
    use rmpv::Value as V;
    Ok(match value {
        V::Nil => serde_json::Value::Null,
        V::Boolean(b) => serde_json::Value::Bool(*b),
        V::Integer(i) => {
            if let Some(u) = i.as_u64() {
                serde_json::Value::from(u)
            } else if let Some(s) = i.as_i64() {
                serde_json::Value::from(s)
            } else {
                // A 64-bit integer that is neither i64 nor u64 cannot
                // exist; rmpv's type allows the shape, so the arm is here
                // rather than an unreachable panic.
                return Err(NotCanonical::NoJsonForm {
                    at: at.to_string(),
                    kind: "an integer outside i64 and u64",
                });
            }
        }
        V::F32(f) => finite(f64::from(*f), at)?,
        V::F64(f) => finite(*f, at)?,
        V::String(s) => match s.as_str() {
            Some(s) => serde_json::Value::String(s.to_string()),
            None => {
                return Err(NotCanonical::NoJsonForm {
                    at: at.to_string(),
                    kind: "a string that is not UTF-8",
                })
            }
        },
        V::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| walk(item, &format!("{at}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        V::Map(pairs) => {
            // rmpv keeps a map as a list of pairs, so a duplicate key is
            // *visible* here in a way it never is after serde_json has
            // parsed it. That is the whole reason this refusal can exist.
            let mut out = serde_json::Map::new();
            for (key, item) in pairs {
                let Some(key) = key.as_str() else {
                    return Err(NotCanonical::NonStringKey {
                        at: at.to_string(),
                        kind: kind_of(key),
                    });
                };
                let child = walk(item, &format!("{at}.{key}"))?;
                if out.insert(key.to_string(), child).is_some() {
                    return Err(NotCanonical::DuplicateKey {
                        at: at.to_string(),
                        key: key.to_string(),
                    });
                }
            }
            serde_json::Value::Object(out)
        }
        V::Binary(_) => {
            return Err(NotCanonical::NoJsonForm {
                at: at.to_string(),
                kind: "a binary blob",
            })
        }
        V::Ext(..) => {
            return Err(NotCanonical::NoJsonForm {
                at: at.to_string(),
                kind: "a msgpack extension value",
            })
        }
    })
}

fn finite(f: f64, at: &str) -> Result<serde_json::Value, NotCanonical> {
    match serde_json::Number::from_f64(f) {
        Some(n) => Ok(serde_json::Value::Number(n)),
        None => Err(NotCanonical::NonFinite {
            at: at.to_string(),
            value: f.to_string(),
        }),
    }
}

fn kind_of(value: &rmpv::Value) -> &'static str {
    use rmpv::Value as V;
    match value {
        V::Nil => "nil",
        V::Boolean(_) => "a boolean",
        V::Integer(_) => "an integer",
        V::F32(_) | V::F64(_) => "a float",
        V::String(_) => "a string",
        V::Binary(_) => "a binary blob",
        V::Array(_) => "an array",
        V::Map(_) => "a map",
        V::Ext(..) => "an extension value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trap one, and the reason this file writes its own serializer:
    /// deliberately unsorted input comes out sorted, whatever
    /// `serde_json`'s `preserve_order` feature is doing to `Map`.
    ///
    /// This test is the mechanism behind consent.md acceptance 6. It runs
    /// in drt's workspace (feature off) and, via the `_preserve-order-test`
    /// feature, in a build where it is on — which is the state dollup's
    /// workspace puts this same code in.
    #[test]
    fn keys_are_sorted_structurally_whatever_map_does() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"zebra":1,"alpha":2,"middle":{"z":1,"a":2}}"#).unwrap();
        assert_eq!(
            String::from_utf8(to_canonical_bytes(&value)).unwrap(),
            r#"{"alpha":2,"middle":{"a":2,"z":1},"zebra":1}"#
        );
    }

    /// The golden vector: one input, the exact bytes, the exact digest.
    ///
    /// Both repositories assert against this rather than each deriving its
    /// own expectation, because "signature failures nobody can debug" is
    /// precisely what happens when two canonicalizers disagree by a comma.
    #[test]
    fn golden_vector() {
        let value: serde_json::Value = serde_json::from_str(GOLDEN_INPUT).unwrap();
        let bytes = to_canonical_bytes(&value);
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), GOLDEN_CANONICAL);
        assert_eq!(hash(&bytes).as_str(), GOLDEN_SHA256);
    }

    /// Absent and `null` are different bytes, which is why consent.md
    /// omits an excluded field rather than blanking it.
    #[test]
    fn null_is_not_absence() {
        let with: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":null}"#).unwrap();
        let without: serde_json::Value = serde_json::from_str(r#"{"a":1}"#).unwrap();
        assert_ne!(hash_value(&with), hash_value(&without));
    }

    /// Trap two: a NaN reaching `serde_json` becomes `null` silently, so
    /// it is refused at the msgpack boundary where it is still visible.
    #[test]
    fn non_finite_floats_are_refused_by_name() {
        let ask = rmpv::Value::Map(vec![("rate".into(), rmpv::Value::F64(f64::NAN))]);
        let e = from_msgpack(&ask).unwrap_err();
        assert!(matches!(e, NotCanonical::NonFinite { .. }), "{e}");
        assert!(e.to_string().contains("ask.rate"), "{e}");

        // And the thing being guarded against really does happen.
        assert!(serde_json::to_value(f64::NAN).unwrap().is_null());
    }

    /// Trap three: duplicate keys are last-wins in JSON and visible in
    /// msgpack, so the refusal lives where the evidence is.
    #[test]
    fn duplicate_keys_are_refused_by_name() {
        let ask = rmpv::Value::Map(vec![
            ("add".into(), rmpv::Value::String("first".into())),
            ("add".into(), rmpv::Value::String("second".into())),
        ]);
        let e = from_msgpack(&ask).unwrap_err();
        assert!(matches!(e, NotCanonical::DuplicateKey { .. }), "{e}");
        assert!(e.to_string().contains("add"), "{e}");
    }

    #[test]
    fn an_ask_that_round_trips_is_accepted() {
        let ask = rmpv::Value::Map(vec![(
            "add".into(),
            rmpv::Value::Array(vec![rmpv::Value::String("example.com".into())]),
        )]);
        let value = from_msgpack(&ask).unwrap();
        assert_eq!(
            String::from_utf8(to_canonical_bytes(&value)).unwrap(),
            r#"{"add":["example.com"]}"#
        );
    }

    #[test]
    fn a_hash_read_off_disk_is_parsed_not_trusted() {
        assert!(Hash::try_from("sha256:00".to_string()).is_err());
        assert!(Hash::try_from(format!("md5:{}", "0".repeat(64))).is_err());
        assert!(Hash::try_from(format!("{HASH_PREFIX}{}", "A".repeat(64))).is_err());
        let ok = Hash::try_from(format!("{HASH_PREFIX}{}", "a".repeat(64))).unwrap();
        assert_eq!(ok.hex(), "a".repeat(64));
    }

    /// The fixture, inline so the bytes and the digest sit beside each
    /// other. `tests/fixtures/canonical.json` is the same input as a file,
    /// for a consumer that wants to read it rather than link this crate.
    const GOLDEN_INPUT: &str = r#"{"realm":"operator.net.domains","ask":{"add":["example.com","a.example.com"]},"node":"root/intake","root_id":"0192f0c1-8000-7000-8000-00000000abcd","valid_until":"2026-09-19T00:00:00Z"}"#;
    const GOLDEN_CANONICAL: &str = r#"{"ask":{"add":["example.com","a.example.com"]},"node":"root/intake","realm":"operator.net.domains","root_id":"0192f0c1-8000-7000-8000-00000000abcd","valid_until":"2026-09-19T00:00:00Z"}"#;
    const GOLDEN_SHA256: &str =
        "sha256:3d67f4ceac8e50ea5b27a5a05388b005bb6b961da90dfd57a40897e311c96792";
}
