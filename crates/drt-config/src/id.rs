//! `root_id` and `created_uuid7`: a uuid7 that this crate can parse,
//! compare and format, and that the **caller** mints.
//!
//! Minting takes the clock reading and the random bytes as arguments rather
//! than reading either. That is not ceremony: `drt` reads its clock and
//! entropy through `drt-platform` so a browser build works at all, dollup
//! reads the host's directly, and a crate both of them depend on must not
//! choose for either. It also keeps this crate free of `rand` and `uuid`,
//! which is the dependency budget dollup was promised.
//!
//! ## surface block
//!
//! - Entry points: [`Uuid7::mint`], a timestamp and ten random bytes to an
//!   id; [`Uuid7::parse`], text to an id; [`Uuid7::unix_ms`], the creation
//!   time back out, which is the whole reason the version is 7.
//! - Configurable: nothing. The layout is RFC 9562 §5.7.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A version 7 UUID: 48 bits of Unix milliseconds, then randomness, with
/// the version and variant bits set.
///
/// Version 7 rather than 4 so that a `created_uuid7` carries its creation
/// time and consent.md §6 needs no `created_at` field beside it. That is
/// the field's *only* job: consent.md is explicit that it is not the
/// request's identity and must never be indexed on, because identity is the
/// content hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Uuid7([u8; 16]);

impl Uuid7 {
    /// Mint from a clock reading and ten random bytes.
    ///
    /// `unix_ms` is truncated to 48 bits, which runs out in AD 10889. The
    /// randomness is the caller's to supply and the caller's to get right:
    /// a `root_id` is minted once per root and is the hinge shipping turns
    /// on, so it is random and never derived from content.
    pub fn mint(unix_ms: u64, random: [u8; 10]) -> Uuid7 {
        let mut bytes = [0u8; 16];
        bytes[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
        bytes[6..].copy_from_slice(&random);
        // Version 7 in the high nibble of byte 6, RFC 9562 variant in the
        // top two bits of byte 8. Both overwrite caller randomness, which
        // is why `random` is ten bytes and not sixteen.
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid7(bytes)
    }

    /// The creation time, milliseconds since the Unix epoch.
    pub fn unix_ms(&self) -> u64 {
        let mut be = [0u8; 8];
        be[2..].copy_from_slice(&self.0[..6]);
        u64::from_be_bytes(be)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Text to an id, hyphenated 8-4-4-4-12, lowercase.
    ///
    /// The version nibble is checked. A version 4 id in a `root_id` field
    /// would work for every comparison and silently lose the creation time
    /// that consent.md §6 relies on, so it is refused where it is read.
    pub fn parse(text: &str) -> Result<Uuid7, BadUuid7> {
        const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
        let mut bytes = [0u8; 16];
        let mut written = 0;
        let mut groups = text.split('-');
        for expected in GROUPS {
            let Some(group) = groups.next() else {
                return Err(BadUuid7::Shape {
                    text: text.to_string(),
                });
            };
            if group.len() != expected {
                return Err(BadUuid7::Shape {
                    text: text.to_string(),
                });
            }
            for pair in group.as_bytes().chunks(2) {
                let hex = std::str::from_utf8(pair).map_err(|_| BadUuid7::Shape {
                    text: text.to_string(),
                })?;
                if hex.bytes().any(|b| b.is_ascii_uppercase()) {
                    return Err(BadUuid7::NotLowercase {
                        text: text.to_string(),
                    });
                }
                bytes[written] = u8::from_str_radix(hex, 16).map_err(|_| BadUuid7::Shape {
                    text: text.to_string(),
                })?;
                written += 1;
            }
        }
        if groups.next().is_some() {
            return Err(BadUuid7::Shape {
                text: text.to_string(),
            });
        }
        let version = bytes[6] >> 4;
        if version != 7 {
            return Err(BadUuid7::WrongVersion {
                text: text.to_string(),
                version,
            });
        }
        Ok(Uuid7(bytes))
    }
}

/// Why text is not a uuid7. Each is a named failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BadUuid7 {
    #[error("'{text}' is not a uuid; expected 8-4-4-4-12 hex digits")]
    Shape { text: String },
    #[error(
        "'{text}': hex digits must be lowercase, or two spellings of one id would compare unequal"
    )]
    NotLowercase { text: String },
    #[error("'{text}' is a version {version} uuid; a root_id is uuid7, which is what carries its creation time")]
    WrongVersion { text: String, version: u8 },
}

impl fmt::Display for Uuid7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl From<Uuid7> for String {
    fn from(id: Uuid7) -> String {
        id.to_string()
    }
}

impl TryFrom<String> for Uuid7 {
    type Error = BadUuid7;
    fn try_from(s: String) -> Result<Uuid7, BadUuid7> {
        Uuid7::parse(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_round_trips_through_text_and_keeps_its_timestamp() {
        let id = Uuid7::mint(1_757_707_440_000, [0xab; 10]);
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(Uuid7::parse(&text).unwrap(), id);
        assert_eq!(id.unix_ms(), 1_757_707_440_000);
    }

    /// The version and variant bits are set over the caller's randomness,
    /// so an all-`ff` random block still produces a well-formed v7.
    #[test]
    fn version_and_variant_are_set_whatever_the_caller_supplied() {
        let id = Uuid7::mint(0, [0xff; 10]);
        assert_eq!(id.as_bytes()[6] >> 4, 7, "version nibble");
        assert_eq!(id.as_bytes()[8] >> 6, 0b10, "RFC 9562 variant");
        assert!(Uuid7::parse(&id.to_string()).is_ok());
    }

    #[test]
    fn ids_sort_by_creation_time() {
        let early = Uuid7::mint(1_000, [0xff; 10]);
        let late = Uuid7::mint(2_000, [0x00; 10]);
        assert!(early < late, "the timestamp leads, so ordering is temporal");
    }

    #[test]
    fn a_v4_uuid_is_refused_where_it_is_read() {
        // A well-formed uuid4: version nibble 4.
        let e = Uuid7::parse("f81d4fae-7dec-41d0-a765-00a0c91e6bf6").unwrap_err();
        assert!(
            matches!(e, BadUuid7::WrongVersion { version: 4, .. }),
            "{e}"
        );
        assert!(Uuid7::parse("not-a-uuid").is_err());
        assert!(Uuid7::parse("0192F0C1-8000-7000-8000-00000000ABCD").is_err());
    }
}
