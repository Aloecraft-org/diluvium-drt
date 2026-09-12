//! `deploy`, `rm` and `commit`: the three directories and the two moves
//! between them.
//!
//! ```text
//!   dlua_dir  ──deploy──▶  live/<name>  ──commit──▶  init/
//!   (authoring)            (what runs)              (delivered content)
//!      init/   ──deploy──▶
//! ```
//!
//! `init/` is what `dollup pull` writes and what `commit` captures. `dlua_dir`
//! is local authoring. `live/` is what runs, and **only** what runs: nothing
//! loads out of `dlua_dir` or `init/` directly, which is what makes `drt
//! deploy` a step an operator can see rather than an implementation detail of
//! `start`.
//!
//! ## surface block
//!
//! - Entry points: [`deploy`], source to `live/`; [`remove`], `live/` gone;
//!   [`commit`], `live/` to `init/` plus the envelope; [`is_deployed`];
//!   [`deployment_name`], which subdirectory of `live/` a root uses.
//! - Configurable values: [`MAX_FILES`] and [`MAX_BYTES`], the bounds a copy
//!   refuses past.
//! - Fan-out: [`Source`] is the two places a deploy can read from, and
//!   [`source_for`] is the one rule that picks between them.
//!
//! **Commit takes from `live/` and never from `dlua_dir`.** Nothing reaches
//! `init/` without having been visible in `live/` first, so a supervisor can
//! review what is about to be committed by looking at what is running. A commit
//! that could capture an editor's buffer would make that review meaningless.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use drt_config::canon::Hash;
use drt_config::envelope::{self, Envelope};
use drt_config::project::ProjectJson;
use drt_config::RootConfig;

use crate::drt_root::{self, Root};

/// The most files one deploy or commit will copy.
///
/// A bound rather than no bound, because the alternative to a named refusal is
/// filling a disk and finding out from the kernel. Generous: a root is source
/// code, and a tree with more files than this is a tree somebody should look
/// at before deploying.
pub const MAX_FILES: usize = 10_000;

/// The most bytes one deploy or commit will copy, in total.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Where a deploy reads from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The profile named a `dlua_dir`: local authoring, which is the
    /// development loop.
    DluaDir(PathBuf),
    /// It did not: `init/`, which is what was delivered.
    Init(PathBuf),
}

impl Source {
    pub fn path(&self) -> &Path {
        match self {
            Source::DluaDir(path) | Source::Init(path) => path,
        }
    }

    /// How to name it in a message. An operator who deployed from the wrong one
    /// needs the word, not the path.
    pub fn describe(&self) -> &'static str {
        match self {
            Source::DluaDir(_) => "dlua_dir",
            Source::Init(_) => "init/",
        }
    }
}

/// Which directory this profile deploys from.
///
/// `dlua_dir` when the resolved profile sets one, `init/` otherwise. One rule,
/// and the profile is what chooses: a debug profile points at `dlua/` and a
/// released root's profile does not, so the same verb does the right thing in
/// both without a flag.
pub fn source_for(root: &Root, config: &RootConfig) -> Source {
    match &config.dlua_dir {
        Some(dir) => Source::DluaDir(root.dir.join(dir)),
        None => Source::Init(root.init()),
    }
}

/// Which subdirectory of `live/` this root deploys into.
///
/// The project's name, so the already-deployed message can say what is
/// deployed. A root with no `project_name` -- legal, and surfaced by audit --
/// falls back to its directory's own name, which is what a human calls it
/// anyway.
pub fn deployment_name(root: &Root, project: Option<&ProjectJson>) -> String {
    project
        .and_then(|p| p.project_name.clone())
        .or_else(|| {
            root.dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "deployment".to_string())
}

/// `live/<name>`.
pub fn live_path(root: &Root, name: &str) -> PathBuf {
    root.live().join(name)
}

/// Is something already deployed here?
pub fn is_deployed(root: &Root, name: &str) -> bool {
    drt_platform::fs::is_dir(live_path(root, name))
}

/// Copy `source` into `live/<name>`, replacing whatever is there.
///
/// Replacing, not merging: a deploy that left a file behind because the new
/// tree no longer has it would run code the source no longer contains, which is
/// the hardest class of difference to see. The caller is responsible for having
/// decided that replacing is wanted -- `start` makes that `--rm`.
pub fn deploy(root: &Root, name: &str, source: &Source) -> Result<Report, String> {
    let into = live_path(root, name);
    if !drt_platform::fs::is_dir(source.path()) {
        return Err(format!(
            "there is nothing to deploy: {} ({}) is not a directory",
            source.path().display(),
            source.describe()
        ));
    }
    drt_platform::fs::remove_dir_all(&into)
        .map_err(|e| format!("cannot clear {}: {e}", into.display()))?;
    copy_tree(source.path(), &into)
}

/// `live/<name>` gone. Missing is success: `rm` of something not deployed has
/// already achieved what it was asked for.
pub fn remove(root: &Root, name: &str) -> Result<(), String> {
    let path = live_path(root, name);
    drt_platform::fs::remove_dir_all(&path)
        .map_err(|e| format!("cannot remove {}: {e}", path.display()))
}

/// Capture `live/<name>` into `init/`, and write the envelope.
///
/// From `live/` and never from `dlua_dir` -- see the module header. "Exactly as
/// much or less than live holds" is this: the copy is what is running, and
/// `commit` is the single writer of the envelope so audit has one current
/// record to check.
pub fn commit(root: &Root, name: &str, project: &ProjectJson) -> Result<Committed, String> {
    let from = live_path(root, name);
    if !drt_platform::fs::is_dir(&from) {
        return Err(format!(
            "nothing is deployed as '{name}', so there is nothing to commit; \
             `drt deploy` first"
        ));
    }
    let into = root.init();
    drt_platform::fs::remove_dir_all(&into)
        .map_err(|e| format!("cannot clear {}: {e}", into.display()))?;
    let report = copy_tree(&from, &into)?;

    let mut envelope = Envelope::new(project.root_id, drt_root::now());
    envelope.files = hashes(&into)?;
    let hash = envelope.hash()?;

    let state = root.state();
    drt_platform::fs::create_dir_all(&state)
        .map_err(|e| format!("cannot create {}: {e}", state.display()))?;
    let path = state.join(envelope::FILENAME);
    let body = serde_json::to_string_pretty(&envelope)
        .map_err(|e| format!("cannot serialize the envelope: {e}"))?;
    drt_platform::fs::write(&path, format!("{body}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    Ok(Committed { report, hash })
}

/// What a deploy or a commit moved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub files: usize,
    pub bytes: u64,
}

/// A commit, and the hash `push` would sign.
#[derive(Debug, Clone, PartialEq)]
pub struct Committed {
    pub report: Report,
    pub hash: Hash,
}

/// The hash of every file under `dir`, keyed by its path relative to `dir`.
///
/// The artifact regime: each file's own bytes. Audit compares this against the
/// envelope, which is why it is a function rather than something `commit` keeps
/// to itself.
pub fn hashes(dir: &Path) -> Result<BTreeMap<String, Hash>, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, prefix)) = stack.pop() {
        for name in drt_platform::fs::read_dir(&path).unwrap_or_default() {
            let child = path.join(&name);
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if drt_platform::fs::is_dir(&child) {
                stack.push((child, relative));
                continue;
            }
            let bytes = drt_platform::fs::read(&child)
                .map_err(|e| format!("cannot read {}: {e}", child.display()))?;
            out.insert(relative, envelope::content_hash(&bytes));
        }
    }
    Ok(out)
}

// depth: the copy, bounded

/// Copy every file under `from` into `to`, making directories as it goes.
///
/// Depth-first over an explicit stack rather than recursion: a deep tree is a
/// deep tree, and a stack overflow is a worse answer than a slow copy. Bounded
/// by [`MAX_FILES`] and [`MAX_BYTES`], each a named refusal, because the
/// alternative is learning the limit from the kernel with a half-written
/// `live/`.
fn copy_tree(from: &Path, to: &Path) -> Result<Report, String> {
    let mut report = Report::default();
    drt_platform::fs::create_dir_all(to)
        .map_err(|e| format!("cannot create {}: {e}", to.display()))?;

    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((source, target)) = stack.pop() {
        drt_platform::fs::create_dir_all(&target)
            .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
        for name in drt_platform::fs::read_dir(&source)
            .map_err(|e| format!("cannot read {}: {e}", source.display()))?
        {
            let child = source.join(&name);
            let into = target.join(&name);
            if drt_platform::fs::is_dir(&child) {
                stack.push((child, into));
                continue;
            }
            let bytes = drt_platform::fs::read(&child)
                .map_err(|e| format!("cannot read {}: {e}", child.display()))?;
            report.files += 1;
            report.bytes += bytes.len() as u64;
            if report.files > MAX_FILES {
                return Err(format!(
                    "more than {MAX_FILES} files under {}; a root is source code, and a tree \
                     this size is one to look at before deploying",
                    from.display()
                ));
            }
            if report.bytes > MAX_BYTES {
                return Err(format!(
                    "more than {} MiB under {}; same reason",
                    MAX_BYTES / (1024 * 1024),
                    from.display()
                ));
            }
            drt_platform::fs::write(&into, &bytes)
                .map_err(|e| format!("cannot write {}: {e}", into.display()))?;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfs::{self, Seeded};

    fn project() -> ProjectJson {
        ProjectJson {
            project_name: Some("my_drt_project".into()),
            ..ProjectJson::new(Seeded::root_id())
        }
    }

    fn dlua_profile() -> RootConfig {
        RootConfig {
            dlua_dir: Some("dlua/".into()),
            ..RootConfig::default()
        }
    }

    /// The development loop the transcript shows: deploy from `dlua_dir`, edit,
    /// and see that `live/` is unchanged until the next deploy. That lag is the
    /// whole reason `live/` exists as a separate directory.
    #[test]
    fn a_deploy_copies_and_an_edit_does_not_reach_live_until_the_next_one() {
        let seeded = testfs::seed(false, &[]);
        let source = source_for(&seeded.root, &dlua_profile());
        assert_eq!(source.describe(), "dlua_dir");

        let report = deploy(&seeded.root, "my_drt_project", &source).unwrap();
        assert_eq!(report.files, 1);
        let live = live_path(&seeded.root, "my_drt_project").join("app.dlua");
        assert_eq!(
            drt_platform::fs::read_to_string(&live).unwrap(),
            "print('hi')\n"
        );

        // Edit, and live/ is still what was deployed.
        seeded.fs.add_file("/r/dlua/app.dlua", "print('edited')\n");
        assert_eq!(
            drt_platform::fs::read_to_string(&live).unwrap(),
            "print('hi')\n",
            "an edit reaches live/ only through a deploy"
        );

        deploy(&seeded.root, "my_drt_project", &source).unwrap();
        assert_eq!(
            drt_platform::fs::read_to_string(&live).unwrap(),
            "print('edited')\n"
        );
    }

    /// A deploy replaces rather than merges: a file the source no longer has
    /// must not keep running.
    #[test]
    fn a_deploy_replaces_rather_than_merging() {
        let seeded = testfs::seed(false, &[("/r/dlua/old.dlua", "print('old')\n")]);
        let source = source_for(&seeded.root, &dlua_profile());
        deploy(&seeded.root, "p", &source).unwrap();
        let old = live_path(&seeded.root, "p").join("old.dlua");
        assert!(drt_platform::fs::exists(&old));

        // Remove it from the source and deploy again.
        drt_platform::fs::remove_file("/r/dlua/old.dlua").unwrap();
        deploy(&seeded.root, "p", &source).unwrap();
        assert!(
            !drt_platform::fs::exists(&old),
            "a stale file in live/ would be code the source no longer contains"
        );
    }

    /// With no `dlua_dir`, a deploy reads `init/` -- a released root, where
    /// there is no authoring directory to read.
    #[test]
    fn without_a_dlua_dir_a_deploy_reads_init() {
        let seeded = testfs::seed(
            false,
            &[("/r/.drt_root/init/app.dlua", "print('delivered')\n")],
        );
        let source = source_for(&seeded.root, &RootConfig::default());
        assert_eq!(source.describe(), "init/");

        deploy(&seeded.root, "p", &source).unwrap();
        assert_eq!(
            drt_platform::fs::read_to_string(live_path(&seeded.root, "p").join("app.dlua"))
                .unwrap(),
            "print('delivered')\n"
        );
    }

    #[test]
    fn deploying_from_nothing_is_a_named_failure() {
        let seeded = testfs::seed(true, &[]);
        let source = source_for(&seeded.root, &RootConfig::default());
        let e = deploy(&seeded.root, "p", &source).unwrap_err();
        assert!(e.contains("nothing to deploy"), "{e}");
        assert!(e.contains("init/"), "it names which directory: {e}");
    }

    #[test]
    fn rm_removes_the_deployment_and_is_idempotent() {
        let seeded = testfs::seed(false, &[]);
        let source = source_for(&seeded.root, &dlua_profile());
        deploy(&seeded.root, "p", &source).unwrap();
        assert!(is_deployed(&seeded.root, "p"));

        remove(&seeded.root, "p").unwrap();
        assert!(!is_deployed(&seeded.root, "p"));
        // Again: success. `rm` of nothing has already done what it was asked.
        remove(&seeded.root, "p").unwrap();
    }

    /// Commit takes from `live/` and not from `dlua_dir`, so nothing reaches
    /// `init/` without having been visible in what runs.
    #[test]
    fn commit_captures_live_and_not_the_editor_s_buffer() {
        let seeded = testfs::seed(false, &[]);
        let source = source_for(&seeded.root, &dlua_profile());
        deploy(&seeded.root, "p", &source).unwrap();

        // An edit that was never deployed.
        seeded
            .fs
            .add_file("/r/dlua/app.dlua", "print('not reviewed')\n");

        let committed = commit(&seeded.root, "p", &project()).unwrap();
        assert_eq!(committed.report.files, 1);
        assert_eq!(
            drt_platform::fs::read_to_string(seeded.root.init().join("app.dlua")).unwrap(),
            "print('hi')\n",
            "what was running is what was committed"
        );

        // And the envelope is there, describing exactly that.
        let text =
            drt_platform::fs::read_to_string(seeded.root.state().join(envelope::FILENAME)).unwrap();
        let envelope: Envelope = serde_json::from_str(&text).unwrap();
        assert_eq!(envelope.root_id, Seeded::root_id());
        assert_eq!(envelope.hash().unwrap(), committed.hash);
        assert!(envelope
            .differences(&hashes(&seeded.root.init()).unwrap())
            .is_empty());
    }

    /// The envelope moves when the committed content moves. Audit's check is
    /// this comparison, so it is asserted rather than assumed.
    #[test]
    fn the_envelope_notices_a_change_under_init() {
        let seeded = testfs::seed(false, &[]);
        let source = source_for(&seeded.root, &dlua_profile());
        deploy(&seeded.root, "p", &source).unwrap();
        let committed = commit(&seeded.root, "p", &project()).unwrap();

        let text =
            drt_platform::fs::read_to_string(seeded.root.state().join(envelope::FILENAME)).unwrap();
        let envelope: Envelope = serde_json::from_str(&text).unwrap();

        seeded
            .fs
            .add_file("/r/.drt_root/init/app.dlua", "tampered\n");
        let differences = envelope.differences(&hashes(&seeded.root.init()).unwrap());
        assert_eq!(
            differences,
            vec![drt_config::envelope::Difference::Changed {
                path: "app.dlua".into()
            }]
        );
        assert_eq!(
            envelope.hash().unwrap(),
            committed.hash,
            "the record is unchanged"
        );
    }

    #[test]
    fn committing_nothing_says_to_deploy_first() {
        let seeded = testfs::seed(false, &[]);
        let e = commit(&seeded.root, "p", &project()).unwrap_err();
        assert!(e.contains("nothing is deployed"), "{e}");
        assert!(e.contains("drt deploy"), "{e}");
    }

    /// Nested directories survive the round trip, which is the case an explicit
    /// stack exists for.
    #[test]
    fn a_nested_tree_survives_deploy_and_commit() {
        let seeded = testfs::seed(
            false,
            &[
                ("/r/dlua/lib/inner/deep.dlua", "return 1\n"),
                ("/r/dlua/lib/side.dlua", "return 2\n"),
            ],
        );
        let source = source_for(&seeded.root, &dlua_profile());
        let report = deploy(&seeded.root, "p", &source).unwrap();
        assert_eq!(report.files, 3);

        commit(&seeded.root, "p", &project()).unwrap();
        let paths: Vec<String> = hashes(&seeded.root.init()).unwrap().into_keys().collect();
        assert_eq!(
            paths,
            ["app.dlua", "lib/inner/deep.dlua", "lib/side.dlua"],
            "relative paths, sorted, with separators kept"
        );
    }

    /// A root with no `project_name` still deploys somewhere nameable.
    #[test]
    fn an_unnamed_project_deploys_under_its_directory_name() {
        let seeded = testfs::seed(false, &[]);
        assert_eq!(
            deployment_name(&seeded.root, Some(&ProjectJson::new(Seeded::root_id()))),
            "r"
        );
        assert_eq!(
            deployment_name(&seeded.root, Some(&project())),
            "my_drt_project"
        );
        assert_eq!(deployment_name(&seeded.root, None), "r");
    }
}
