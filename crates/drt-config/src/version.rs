//! One spelling for a version, so a pin written under one grammar compares
//! equal to a binary reporting the other.
//!
//! DRT's candidates were tagged `vX.Y.ZrcN` through v0.6.0rc1; from the next
//! cut they are `vX.Y.Z-rc.N` (`doc/ALIGNMENT.md` §1). A root's `drt` pin is
//! the tag body and the comparison is a string compare, so the first binary
//! built under the new spelling would mismatch every root dollup has
//! written -- and a SemVer parser cannot rescue them, because `0.5.0rc9` is
//! not SemVer at all (§10). So: a normaliser, for one cycle.
//!
//! Pure. Removable once no root pins the old spelling -- delete this module
//! and let `resolve::check_pin` compare strings again.
//!
//! ## surface block
//!
//! - Entry points: [`same`], the comparison; [`canonical`], the spelling it
//!   compares.
//! - Configurable values: [`LEGACY_KIND`], the one prerelease kind the old
//!   grammar was ever used with.
//! - Fan-out: none; one legacy shape maps to one canonical one, and every
//!   other string is left as it is.

/// The only prerelease kind the old grammar was ever used with: every
/// candidate before the cutover was `rcN`, never `aN`, `bN` or `devN`, so
/// those are not rewritten -- a spelling nobody ever cut is not one to guess
/// at.
pub const LEGACY_KIND: &str = "rc";

/// Whether two pin spellings name the same version.
pub fn same(a: &str, b: &str) -> bool {
    canonical(a) == canonical(b)
}

/// The new spelling of a version written in the old one, and anything else
/// unchanged: `0.5.0rc9` is `0.5.0-rc.9`; `0.5.0-rc.9`, `0.5.0` and text
/// that is not a version at all come back as they were.
pub fn canonical(spelling: &str) -> String {
    let Some((base, rest)) = split_base(spelling) else {
        return spelling.to_string();
    };
    match rest.strip_prefix(LEGACY_KIND) {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{base}-{LEGACY_KIND}.{n}")
        }
        _ => spelling.to_string(),
    }
}

// depth: `X.Y.Z` off the front, and what follows it.
fn split_base(spelling: &str) -> Option<(&str, &str)> {
    let bytes = spelling.as_bytes();
    let mut i = 0;
    for component in 0..3 {
        if component > 0 {
            if bytes.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
        let start = i;
        while bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    Some(spelling.split_at(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_spelling_and_the_new_one_are_the_same_version() {
        assert!(same("0.5.0rc9", "0.5.0-rc.9"));
        assert!(same("0.5.0-rc.9", "0.5.0rc9"));
        assert!(same("0.6.0rc1", "0.6.0rc1"));
        assert!(same("0.6.0", "0.6.0"));
    }

    #[test]
    fn different_versions_stay_different() {
        assert!(!same("0.5.0rc9", "0.5.0"));
        assert!(!same("0.5.0rc9", "0.5.0rc10"));
        assert!(!same("0.5.0rc9", "0.5.1rc9"));
        assert!(!same("0.5.0-rc.9", "0.5.0-dev.9"));
    }

    /// One shape is rewritten and nothing else is, so a string this was
    /// never meant for passes through untouched rather than half-parsed.
    #[test]
    fn only_the_one_legacy_shape_is_rewritten() {
        assert_eq!(canonical("0.5.0rc9"), "0.5.0-rc.9");
        assert_eq!(canonical("0.5.0-rc.9"), "0.5.0-rc.9");
        assert_eq!(canonical("0.5.0"), "0.5.0");
        assert_eq!(canonical("0.5.0rc"), "0.5.0rc");
        assert_eq!(canonical("0.5.0a1"), "0.5.0a1");
        assert_eq!(canonical("v0.5.0rc9"), "v0.5.0rc9");
        assert_eq!(canonical("garbage"), "garbage");
        assert_eq!(canonical(""), "");
    }
}
