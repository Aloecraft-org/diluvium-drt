//! Stamps the embedded diluvium revision into the binary.
//!
//! ## surface block
//!
//! - `main`: the only entry point. Emits `DRT_DILUVIUM_REV` and the rerun
//!   directives.
//! - [`LOCK_DEPTH`]: how far up to look for the workspace `Cargo.lock`.
//! - [`UNKNOWN`]: what is emitted when the lock cannot be read or does not
//!   pin diluvium by revision. A path-dependency build is the normal case
//!   for that, not a failure.
//!
//! Why this exists: `requires.diluvium` in a dollup package is checked
//! against *which diluvium is inside*, and until now that fact lived only
//! in `BUILDINFO.txt` — a sidecar the release workflow writes by grepping
//! `Cargo.lock`. A binary someone copied off a machine carried no answer at
//! all. `doc/Release.md`'s rule is that the compatibility fact travels with
//! the bytes; a fact in a file beside the bytes does not travel with them.

use std::path::PathBuf;

/// `crates/drt/` -> workspace root is two hops. Searched upward rather than
/// hard-coded to one path so a vendored or relocated build still finds it.
const LOCK_DEPTH: usize = 4;

/// Emitted when the revision cannot be established. A consumer reading this
/// knows it does not know, which is the point — an absent field and a wrong
/// field are both worse.
const UNKNOWN: &str = "unknown";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let rev = lock_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| revision_of(&text))
        .unwrap_or_else(|| UNKNOWN.to_string());
    println!("cargo:rustc-env=DRT_DILUVIUM_REV={rev}");
}

fn lock_path() -> Option<PathBuf> {
    let mut dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    for _ in 0..LOCK_DEPTH {
        let lock = dir.join("Cargo.lock");
        if lock.is_file() {
            println!("cargo:rerun-if-changed={}", lock.display());
            return Some(lock);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

// depth: the two shapes a `[[package]]` block can take
//
// A git dependency's `source` ends in `#<sha>`; a path dependency has no
// `source` line at all. Only the first can be reported, and the second is a
// normal way to build this repository rather than a fault, so it answers
// `unknown` and says nothing further.
fn revision_of(lock: &str) -> Option<String> {
    let mut in_diluvium = false;
    for line in lock.lines() {
        if line.starts_with("[[package]]") {
            in_diluvium = false;
        } else if line == "name = \"diluvium\"" {
            in_diluvium = true;
        } else if in_diluvium {
            if let Some(rest) = line.strip_prefix("source = \"git+") {
                return rest
                    .rsplit_once('#')
                    .map(|(_, sha)| sha.trim_end_matches('"').to_string());
            }
        }
    }
    None
}
