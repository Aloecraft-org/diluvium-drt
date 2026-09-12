//! Every deployed config shape still parses to what it parsed to before
//! (`doc/Plan-2026-09.md` §3.8).
//!
//! `drt-config` is shared by everything, and a round that adds fields to it
//! -- the `numeric` block, the `data` connector's `columns` list -- adds
//! them beside the existing ones rather than by reworking a shared path.
//! This is the check that says so: the corpus in
//! `crates/drt-config/tests/corpus/` is loaded through the real loader and
//! diffed against a snapshot of what it parsed to. **Any loader change that
//! fails a corpus file is wrong regardless of what it enables.**
//!
//! The files live beside `drt-config` because they are that crate's
//! evidence; this test lives here because `load` does, and `drt-config`
//! cannot depend on `drt`.
//!
//! ## surface block
//!
//! - [`the_corpus_parses_to_what_it_parsed_to_before`]: the only entry
//!   point.
//! - [`CORPUS`], [`SNAPSHOTS`]: where the files and their snapshots are,
//!   relative to this crate.
//! - [`UPDATE`]: the environment variable that rewrites a snapshot instead
//!   of comparing it.
//! - [`PLACEHOLDER`]: what the corpus directory is written as inside a
//!   snapshot, since a refusal names the file it refused.
//!
//! A refusal is snapshotted like any other outcome, and several corpus
//! files are here *because* they are refused: `11-tunnel-and-relay`'s relay
//! carries blank keys, and a blank key is a closed door. "It still refuses,
//! for the same reason" is as much a fact about the loader as "it still
//! parses to this", and the shape that stops being refused is the
//! regression nobody would otherwise see. The outcome recorded is therefore
//! `load` **and** `validate`: `load` is serde, which cannot see a key that
//! is present and empty, and that blank key is the whole reason that file
//! is here.
//!
//! **Every file is read in every build**, and there is no skip list. There
//! used to be one -- a `NEEDS` table naming the feature each file's block
//! wanted -- because the `.host.lua` mapper was compiled per feature and
//! answered "unknown key" for a block the build had not been compiled to
//! know. Nothing in the JSON path is gated: `RootConfig` carries every
//! block's field in every build, so a `--features slim` run reads the
//! wireguard corpus and gets the same structure `--all-features` does. The
//! snapshots are build-independent, which is strictly more than the old
//! arrangement could say.

use std::path::{Path, PathBuf};

const CORPUS: &str = "../drt-config/tests/corpus";
const SNAPSHOTS: &str = "../drt-config/tests/corpus/snapshots";
const UPDATE: &str = "DRT_CORPUS_UPDATE";
const PLACEHOLDER: &str = "<corpus>";

fn here(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// The corpus files, sorted. `README.md` and `snapshots/` are not entries.
fn corpus_files() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(here(CORPUS))
        .expect("the corpus directory exists")
        .map(|e| e.expect("a readable corpus entry"))
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "README.md")
        .collect();
    names.sort();
    names
}

/// What the file loaded to, as text a diff can be read from: the parsed
/// structure, or the refusal.
///
/// `supervisor` resolves against the config's own directory and a refusal
/// names the file it refused, so either carries this checkout's absolute
/// path; it is written back out as [`PLACEHOLDER`] so a snapshot means the
/// same thing on every machine.
fn snapshot_of(outcome: &Result<drt_config::RootConfig, String>, corpus_dir: &Path) -> String {
    let text = match outcome {
        Ok(config) => format!("parsed to:\n{config:#?}\n"),
        Err(e) => format!("refused with:\n{e}\n"),
    };
    text.replace(
        &format!("{}/", corpus_dir.display()),
        &format!("{PLACEHOLDER}/"),
    )
    .replace(&corpus_dir.display().to_string(), PLACEHOLDER)
}

#[test]
fn the_corpus_parses_to_what_it_parsed_to_before() {
    let corpus_dir = here(CORPUS);
    let snapshot_dir = here(SNAPSHOTS);
    std::fs::create_dir_all(&snapshot_dir).expect("the snapshot directory is writable");
    let updating = std::env::var_os(UPDATE).is_some();

    let mut loaded = 0;
    let mut written: Vec<String> = Vec::new();
    let mut wrong: Vec<String> = Vec::new();

    for name in corpus_files() {
        let path = corpus_dir.join(&name);
        // `load` validates, so a refusal is recorded like any other
        // outcome -- several of these files are here to be refused.
        let outcome = drt::config::load(Some(&path));
        loaded += 1;

        let actual = snapshot_of(&outcome, &corpus_dir);
        let snapshot = snapshot_dir.join(format!("{name}.txt"));
        match std::fs::read_to_string(&snapshot) {
            // Snapshot on first run: a file with no record yet gets one,
            // and the run passes. The record is the commit, not the test.
            Err(_) => {
                std::fs::write(&snapshot, &actual).expect("the snapshot is writable");
                written.push(name);
            }
            Ok(_) if updating => {
                std::fs::write(&snapshot, &actual).expect("the snapshot is writable");
                written.push(name);
            }
            Ok(expected) if expected != actual => {
                wrong.push(format!(
                    "---- {name} ----\n\
                     before, it {expected}\n\
                     now, it {actual}"
                ));
            }
            Ok(_) => {}
        }
    }

    assert_eq!(
        loaded,
        corpus_files().len(),
        "every corpus file is read in every build; none is skipped"
    );
    for name in &written {
        eprintln!("snapshot written for {name}");
    }
    assert!(
        wrong.is_empty(),
        "{} corpus file(s) load differently than they did:\n\n{}\n\
         Every shipped install runs a config this loader wrote, and a \
         loader change that does this is wrong regardless of what it \
         enables. If the change was deliberate, re-record with {UPDATE}=1 \
         and read the diff before committing it.",
        wrong.len(),
        wrong.join("\n")
    );
}
