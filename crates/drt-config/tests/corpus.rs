//! The corpus is complete and verbatim (`doc/Plan-2026-09.md` §3.8).
//!
//! ## surface block
//!
//! - [`the_copies_are_byte_identical_to_their_sources`]: every corpus file
//!   named after a repository path still equals that file.
//! - [`every_shipped_config_shape_is_in_the_corpus`]: every shipped shape
//!   has a copy here, so a new one cannot ship without being covered.
//! - [`CORPUS`]: where the files live, relative to this crate.
//! - [`SHIPPED`]: where a shipped shape can be, relative to the repository
//!   root — the fan-out point. A new source directory is added here, and
//!   nowhere else in this file.
//! - [`SHIPPED_SUFFIX`], [`SHIPPED_EXACT`]: what counts as a shape in those
//!   directories.
//! - [`SEP`]: how a repository path is written as a corpus filename.
//!
//! No loader is involved, deliberately: this half of the check must hold in
//! every build, including one compiled without `relay`, `turn` or
//! `wireguard`, which cannot parse half the corpus. The half that parses is
//! `crates/drt/tests/config_corpus.rs`.

use std::path::{Path, PathBuf};

const CORPUS: &str = "tests/corpus";

/// Directories that carry a config shape an install actually runs.
const SHIPPED: &[&str] = &["examples"];

/// A shape is any `*.host.lua` under [`SHIPPED`] …
const SHIPPED_SUFFIX: &str = ".host.lua";

/// … plus these named files, which are shapes without being `.host.lua`.
const SHIPPED_EXACT: &[&str] = &[
    "examples/deployment.json",
    "examples/19-a-tunnel-a-program-can-use/park.json",
    "examples/19-a-tunnel-a-program-can-use/claim.json",
    "examples/24-wireguard-userspace/laptop.json",
    "examples/24-wireguard-userspace/fetchpoint.json",
];

/// `/` in a repository path is `__` in a corpus filename. A corpus file
/// with no `__` was captured from outside the repository and has no source
/// to compare against; `README.md` says which and why.
const SEP: &str = "__";

fn repo_root() -> PathBuf {
    // crates/drt-config -> crates -> the root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two directories below the repository root")
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS)
}

/// The corpus files, by name, sorted. `README.md` and `snapshots/` are not
/// corpus entries.
fn corpus_files() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(corpus_dir())
        .expect("the corpus directory exists")
        .map(|e| e.expect("a readable corpus entry"))
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "README.md")
        .collect();
    names.sort();
    names
}

// depth: walking `examples/` without pulling a directory-walker into a
// crate that has no other use for one.
fn shipped_shapes() -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(SHIPPED_SUFFIX))
            {
                out.push(
                    path.strip_prefix(root)
                        .expect("walked from the root")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }

    let root = repo_root();
    let mut found = Vec::new();
    for dir in SHIPPED {
        walk(&root, &root.join(dir), &mut found);
    }
    for exact in SHIPPED_EXACT {
        if root.join(exact).is_file() {
            found.push((*exact).to_string());
        }
    }
    found.sort();
    found
}

/// A copy that has drifted from its source is not evidence about anything:
/// it says the loader still parses a file no install has.
#[test]
fn the_copies_are_byte_identical_to_their_sources() {
    let root = repo_root();
    let corpus = corpus_dir();
    let mut checked = 0;
    for name in corpus_files() {
        if !name.contains(SEP) {
            continue; // captured from outside the repository; no source
        }
        let source = root.join(name.replace(SEP, "/"));
        assert!(
            source.is_file(),
            "{name} is named after {}, which does not exist. Either the \
             source moved (rename the copy) or it was deleted (drop the copy).",
            source.display()
        );
        let copied = std::fs::read(corpus.join(&name)).expect("the copy reads");
        let original = std::fs::read(&source).expect("the source reads");
        assert!(
            copied == original,
            "{name} has drifted from {}. The corpus is a copy, not a \
             variant: re-copy it, and if the change was deliberate then \
             re-snapshot what it now parses to.",
            source.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "the corpus copied nothing");
}

/// A config shape that ships without a copy here is a shape the loader is
/// free to break, which is the whole failure this corpus exists to stop.
#[test]
fn every_shipped_config_shape_is_in_the_corpus() {
    let present = corpus_files();
    let missing: Vec<String> = shipped_shapes()
        .into_iter()
        .filter(|path| !present.contains(&path.replace('/', SEP)))
        .collect();
    assert!(
        missing.is_empty(),
        "these shipped config shapes have no copy in {CORPUS}: {missing:#?}\n\
         Copy each one in, naming it with `/` written `{SEP}`, and run the \
         snapshot test to record what it parses to."
    );
}
