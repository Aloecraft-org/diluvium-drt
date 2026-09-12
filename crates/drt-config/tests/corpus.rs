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

/// The shapes with a copy here. Every config file under [`SHIPPED`] is
/// either in this list or in [`NOT_COVERED`], and
/// [`every_shipped_config_is_accounted_for`] fails if one is in neither --
/// which is what keeps "a new shape cannot ship uncovered" true now that
/// there is no suffix to recognise a shape by.
///
/// Until this round every shape was a `*.host.lua` and the rule was that
/// suffix plus five named JSON files. `.host.lua` is gone and a bare
/// `.json` recognises `meta.json` and every example's own config alike, so
/// the rule is now two lists and a test that says they are exhaustive.
const SHIPPED_EXACT: &[&str] = &[
    "examples/deployment.json",
    "examples/11-tunnel-and-relay/rendezvous.json",
    "examples/19-a-tunnel-a-program-can-use/park.json",
    "examples/19-a-tunnel-a-program-can-use/claim.json",
    "examples/19-a-tunnel-a-program-can-use/rendezvous.json",
    "examples/20-turn-relay/app.json",
    "examples/20-turn-relay/open.json",
    "examples/20-turn-relay/turn.json",
    "examples/21-wireguard/fp.json",
    "examples/21-wireguard/hub-unroutable.json",
    "examples/21-wireguard/hub.json",
    "examples/21-wireguard/rendezvous.json",
    "examples/21-wireguard/wrong.json",
    "examples/22-wireguard-interface/fp.json",
    "examples/24-wireguard-userspace/laptop.json",
    "examples/24-wireguard-userspace/fetchpoint.json",
    "examples/rendezvous/rendezvous.json",
];

/// Config files under [`SHIPPED`] with no copy here, each one a gap rather
/// than a decision.
///
/// These are exactly the shapes the old `*.host.lua` rule never reached,
/// listed rather than left implicit: the suffix rule made the corpus look
/// complete while covering the network blocks and nothing else. Every entry
/// here is a config an example actually runs and would be worth covering.
/// Adding one is: copy the file in, drop its line from here.
///
/// **Not a place to put a new config to quiet the test.** A file added here
/// is a shape no gate watches.
const NOT_COVERED: &[&str] = &[
    "examples/02-capabilities/with-fs.json",
    "examples/04-files/read-only.json",
    "examples/04-files/readwrite.json",
    "examples/05-calling-a-rest-api-live/allowlist.json",
    "examples/05-calling-a-rest-api/allowlist.json",
    "examples/06-budgets/bounded.json",
    "examples/06-budgets/tight.json",
    "examples/07-sql/read-only.json",
    "examples/07-sql/readwrite.json",
    "examples/08-spawn-and-hibernation/app.json",
    "examples/10-ssh-exec/deploy.json",
    "examples/13-stun-server/stun1.json",
    "examples/13-stun-server/stun2.json",
    "examples/15-sending-mail/deploy.json",
    "examples/16-exec/deploy.json",
    "examples/17-serving-http/app.json",
    "examples/18-capability-menu/auditor.json",
    "examples/18-capability-menu/ungranted.json",
    "examples/18-capability-menu/worker.json",
    "examples/19-a-tunnel-a-program-can-use/device.json",
    "examples/23-reading-parquet/app.json",
    "examples/24-wireguard-userspace/device.json",
];

/// The examples harness's own manifest, in every example directory. Not a
/// drt config and not a shape: it is what `script/examples.sh` reads.
const NOT_A_CONFIG: &str = "meta.json";

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

/// Every config file under [`SHIPPED`]: every `.json` that is not
/// [`NOT_A_CONFIG`]. Membership is decided by the two lists above, not
/// here; this is only what is on disk.
fn shipped_configs() -> Vec<String> {
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
                .is_some_and(|n| n.ends_with(".json") && n != NOT_A_CONFIG)
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
    found.sort();
    found
}

/// The shapes that must have a copy here.
fn shipped_shapes() -> Vec<String> {
    let root = repo_root();
    let mut found: Vec<String> = SHIPPED_EXACT
        .iter()
        .filter(|e| root.join(e).is_file())
        .map(|e| (*e).to_string())
        .collect();
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

/// The two lists are exhaustive, so a config added to `examples/` cannot
/// pass unnoticed.
///
/// This is what `*.host.lua` used to do for free. A shape was recognisable
/// by its suffix, so a new one had a copy here or the test above failed.
/// Every shape is `.json` now and so is `meta.json`, so the suffix decides
/// nothing and the lists do -- and a list only works while something makes
/// you update it. That is this test.
#[test]
fn every_shipped_config_is_accounted_for() {
    let unlisted: Vec<String> = shipped_configs()
        .into_iter()
        .filter(|p| !SHIPPED_EXACT.contains(&p.as_str()) && !NOT_COVERED.contains(&p.as_str()))
        .collect();
    assert!(
        unlisted.is_empty(),
        "these config files under {SHIPPED:?} are in neither SHIPPED_EXACT \
         nor NOT_COVERED: {unlisted:#?}\n\
         Add each to SHIPPED_EXACT and copy it into {CORPUS} (preferred), \
         or to NOT_COVERED, which says out loud that no gate watches it."
    );

    // A path that no longer exists is a list that has stopped describing
    // the tree -- the quiet way a rule like this rots.
    let root = repo_root();
    let stale: Vec<&str> = SHIPPED_EXACT
        .iter()
        .chain(NOT_COVERED)
        .filter(|p| !root.join(p).is_file())
        .copied()
        .collect();
    assert!(
        stale.is_empty(),
        "these listed paths do not exist: {stale:#?}\n\
         A shape that moved is renamed in the list; one that was deleted is \
         dropped from it, with its copy."
    );
}
