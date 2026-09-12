//! The module-name rule: what `require` may name, and which file answers to
//! it.
//!
//! Here rather than in drt because two programs enforce it. drt's loader
//! walks a node's directory and builds the table `require` reads
//! (`doc/Modules.md`); dollup refuses a package at pull whose files could not
//! be reached by any name. A name one accepted and the other refused could
//! not exist, which is the same reason [`crate::project::RESERVED`] lives
//! beside this.
//!
//! Pure: no filesystem, no clock, no entropy. Every function here is a string
//! rule, so either side can apply it to a path it is only reading about.
//!
//! ## surface block
//!
//! - Entry points: [`refuse_name`], the rule itself; [`name_for_path`] and
//!   [`paths_for_name`], the two directions between a name and a file;
//!   [`module_extension`], which says whether a file is a module at all.
//! - Configurable values: [`RESERVED_COMPONENT`], [`SOURCE_EXTENSIONS`],
//!   [`BYTECODE_EXTENSION`].
//! - Fan-out: [`refuse_name`]'s arms, one per way a name can be wrong.

use std::fmt::Write as _;

/// The first component no module may have.
///
/// `stdlib:<name>` is how a program shipped inside the binary is named as an
/// entry ([`crate::resolve::Entry`]), and a module able to shadow that
/// spelling would make one name mean two things.
pub const RESERVED_COMPONENT: &str = "stdlib";

/// The extensions a module's source may have, in the order a name prefers
/// them.
///
/// Both, because both are guest source everywhere else: `RepoFormat.md`
/// admits either in a package and dollup's source-only check takes either. A
/// loader that walked only `.dlua` would leave `util.lua` sitting in the
/// directory answering to nothing, which is the failure this list exists to
/// prevent.
pub const SOURCE_EXTENSIONS: &[&str] = &["dlua", "lua"];

/// Compiled module source. Preferred over [`SOURCE_EXTENSIONS`] when both are
/// present -- an opted-in package carries bytecode and the loader runs it.
///
/// **drt refuses it today**, by name, because it has no bytecode verifier and
/// the guest-side `load` it would go through accepts a binary chunk. The rule
/// is here so the two sides agree on what it *will* mean; `doc/Modules.md`
/// carries the refusal and why.
pub const BYTECODE_EXTENSION: &str = "dluac";

/// A character one name component may hold. The `.` is the separator between
/// components and so is not one of these.
pub fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Why this is not a module name, or `None` if it is.
///
/// The message is the one a person reads, on either side of the seam, so it
/// says what is wrong rather than which rule fired.
pub fn refuse_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a module name may not be empty".into());
    }
    if name.contains("..") {
        return Some("`..` is not a module name component".into());
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Some("a module name may not begin or end with `.`".into());
    }
    if !name.chars().all(|c| is_name_char(c) || c == '.') {
        return Some(
            "a module name holds letters, digits, `_` and the `.` between components".into(),
        );
    }
    if name.split('.').next() == Some(RESERVED_COMPONENT) {
        return Some(format!(
            "`{RESERVED_COMPONENT}` is reserved -- a stdlib program is reached by the \
             `{RESERVED_COMPONENT}:` entry spelling, never by require"
        ));
    }
    None
}

/// Which module extension `relative` has, or `None` if it is not a module
/// file at all.
///
/// A `.json` beside the entry is a config, a `.md` is prose, and neither is
/// something `require` should find.
pub fn module_extension(relative: &str) -> Option<&'static str> {
    let last = relative.rsplit('/').next()?;
    let (_, extension) = last.rsplit_once('.')?;
    if extension == BYTECODE_EXTENSION {
        return Some(BYTECODE_EXTENSION);
    }
    SOURCE_EXTENSIONS.iter().copied().find(|e| *e == extension)
}

/// The module name a file answers to: `db/claims.dlua` is `db.claims`.
///
/// `relative` is `/`-separated and relative to the node's own directory, on
/// every platform -- a module name is not a path and must not change shape
/// with the host.
///
/// A component may not itself contain a `.`, which is the rule that keeps
/// this one-to-one: with `my.helper.dlua` allowed, `my.helper` would name
/// both it and `my/helper.dlua`, and one of the two would win silently.
pub fn name_for_path(relative: &str) -> Result<String, String> {
    let extension =
        module_extension(relative).ok_or_else(|| format!("{relative}: not a module file"))?;
    let stem = &relative[..relative.len() - extension.len() - 1];
    let mut name = String::with_capacity(stem.len());
    for component in stem.split('/') {
        if component.is_empty() || !component.chars().all(is_name_char) {
            return Err(format!(
                "{relative}: `{component}` cannot be part of a module name, which holds \
                 letters, digits and `_` between the dots"
            ));
        }
        if !name.is_empty() {
            name.push('.');
        }
        let _ = write!(name, "{component}");
    }
    if let Some(why) = refuse_name(&name) {
        return Err(format!("{relative}: {why}"));
    }
    Ok(name)
}

/// Every file a name could be answered by, in the order preferred.
///
/// Bytecode first, then source in [`SOURCE_EXTENSIONS`] order. A caller that
/// finds more than one of these present has an ambiguous node and should say
/// so rather than choose.
pub fn paths_for_name(name: &str) -> Result<Vec<String>, String> {
    if let Some(why) = refuse_name(name) {
        return Err(why);
    }
    let stem = name.replace('.', "/");
    let mut out = Vec::with_capacity(SOURCE_EXTENSIONS.len() + 1);
    out.push(format!("{stem}.{BYTECODE_EXTENSION}"));
    for extension in SOURCE_EXTENSIONS {
        out.push(format!("{stem}.{extension}"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rule_refuses_what_the_design_says_it_refuses() {
        for bad in [
            "",
            "..secret",
            "a..b",
            "/etc/x",
            "util/enc",
            ".leading",
            "trailing.",
            "has space",
            "stdlib",
            "stdlib.anything",
        ] {
            assert!(refuse_name(bad).is_some(), "should refuse {bad:?}");
        }
        for good in ["a", "enc", "util.enc", "db.claims", "a_b.c1", "stdlibx.y"] {
            assert!(refuse_name(good).is_none(), "should accept {good:?}");
        }
    }

    #[test]
    fn a_path_and_a_name_are_the_same_fact_from_two_sides() {
        assert_eq!(name_for_path("db/claims.dlua").unwrap(), "db.claims");
        assert_eq!(name_for_path("enc.lua").unwrap(), "enc");
        assert_eq!(name_for_path("a/b/c.dluac").unwrap(), "a.b.c");
        assert_eq!(
            paths_for_name("db.claims").unwrap(),
            ["db/claims.dluac", "db/claims.dlua", "db/claims.lua"]
        );
    }

    /// Both spellings are modules, because both are guest source everywhere
    /// else in the format.
    #[test]
    fn dlua_and_lua_are_both_module_files_and_nothing_else_is() {
        assert_eq!(module_extension("a.dlua"), Some("dlua"));
        assert_eq!(module_extension("a.lua"), Some("lua"));
        assert_eq!(module_extension("a.dluac"), Some("dluac"));
        assert_eq!(module_extension("a.json"), None);
        assert_eq!(module_extension("README.md"), None);
        assert_eq!(module_extension("noextension"), None);
    }

    #[test]
    fn a_dot_inside_a_component_is_refused_because_it_would_be_ambiguous() {
        let e = name_for_path("my.helper.dlua").unwrap_err();
        assert!(e.contains("my.helper"), "{e}");
        let e = name_for_path("a-b.dlua").unwrap_err();
        assert!(e.contains("a-b"), "{e}");
    }

    #[test]
    fn the_reserved_component_is_refused_from_a_path_too() {
        let e = name_for_path("stdlib/relay.dlua").unwrap_err();
        assert!(e.contains("reserved"), "{e}");
        assert!(paths_for_name("stdlib.relay").is_err());
    }
}
