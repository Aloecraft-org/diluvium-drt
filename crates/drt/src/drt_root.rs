//! A root on disk: where it is, what is in it, and the two values this
//! process has to supply that `drt-config` refuses to read for itself.
//!
//! `drt-config` holds the formats and `resolve`; it opens no path, which is
//! what lets `dollup audit` report the runtime's answer rather than an
//! approximation of it. This file is the other half: the IO that fills
//! [`drt_config::resolve::ResolveInputs`], and the writes that `start`
//! performs. Everything goes through `drt_platform::fs`, so a page reaches
//! a root it seeded in memory exactly as a shell reaches one on disk.
//!
//! Named `drt_root` for the directory it is about, because `roots.rs` beside
//! it is already PEM trust anchors: two modules spelled `root` and `roots`
//! meaning a deployment and a certificate authority is a collision waiting to
//! be tripped over, and one of them may as well say which it is.
//!
//! ## surface block
//!
//! - Entry points: [`discover`], cwd or `--root` to a located root;
//!   [`Root::read`], the root's files into resolver inputs; [`Root::nesting`],
//!   the only walk upward; [`Root::write_consent`]; [`now`] and [`mint`], the
//!   clock reading and the entropy `drt-config` takes as arguments.
//! - Configurable values: none of its own. Every name inside a root is
//!   `drt_config::project`'s constant, so the layout is declared once.
//! - Fan-out: [`Nesting`] is the three answers `allow_nested` has.
//!
//! **Two different walks, and they must not be merged.** Discovery does
//! *not* walk up: `.drt_root/` is looked for in the working directory and
//! nowhere else, so an unclaimed subdirectory of a root is not in that root
//! and `cd build && drt start` cannot silently start somebody else's
//! deployment. The nesting *check* does walk up, because "you are inside
//! another root" is worth saying — and the **inner** root's `allow_nested`
//! governs it, since the outer root's config cannot be relied on to be
//! readable by whoever is running here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use drt_config::consent::ConsentJson;
use drt_config::id::Uuid7;
use drt_config::project::{self, AllowNested, ProjectJson};
use drt_config::resolve::{Overrides, Requested, ResolveInputs, RootInputs};
use drt_config::time::Timestamp;
use drt_config::RootConfig;

/// What a `drt` pin in `project.json` is compared against.
///
/// The release tag when the build was cut as one, and the crate version
/// otherwise. Those differ, and the difference is the point: by the version
/// scheme (`doc/Gap-Release.md`, "one thing not to re-litigate") a candidate
/// is tagged `X.Y.ZrcN` while its crates stay at `X.Y.Z`. A binary reporting
/// `CARGO_PKG_VERSION` therefore calls itself `0.5.0` whether it was cut as
/// rc8, rc9 or the release -- so a root pinned to `0.5.0rc9` could never
/// start, and a root pinned to `0.5.0` could not tell two candidates apart.
/// Prerelease pins did not work at all.
///
/// The tag already travels with the bytes: `build.rs` re-exports
/// `DRT_RELEASE_TAG` and `drt buildinfo` prints it, which is
/// `doc/Release.md`'s rule that a compatibility fact travels with the binary
/// rather than in a file beside it. This is the pin reading a fact that was
/// already there.
///
/// A local build stamps no tag and falls back, so a development tree behaves
/// exactly as it did.
pub fn binary_version() -> String {
    pin_string(option_env!("DRT_RELEASE_TAG"), env!("CARGO_PKG_VERSION"))
}

/// The transform, apart from the compile-time reads so it can be tested.
///
/// The leading `v` is the tag's and not the version's: `project.json` pins
/// `0.5.0rc9` and the tag is `v0.5.0rc9`. An empty variable is a rehearsal's
/// blank input and means no tag, the same reading `build.rs` gives it.
fn pin_string(tag: Option<&str>, crate_version: &str) -> String {
    tag.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.strip_prefix('v').unwrap_or(t).to_string())
        .unwrap_or_else(|| crate_version.to_string())
}

/// A located root: the directory that contains `.drt_root/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// The project directory — the parent of `.drt_root/`, and what
    /// `dlua_dir` is relative to.
    pub dir: PathBuf,
}

impl Root {
    /// `<dir>/.drt_root`.
    pub fn meta(&self) -> PathBuf {
        self.dir.join(project::ROOT_DIR)
    }

    pub fn project_json(&self) -> PathBuf {
        self.meta().join("project.json")
    }

    pub fn consent_json(&self) -> PathBuf {
        self.meta().join("consent.json")
    }

    pub fn init(&self) -> PathBuf {
        self.meta().join(project::INIT_DIR)
    }

    pub fn live(&self) -> PathBuf {
        self.meta().join(project::LIVE_DIR)
    }

    pub fn log(&self) -> PathBuf {
        self.meta().join(project::LOG_DIR)
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.meta().join(project::PROFILE_DIR)
    }

    /// Runtime-owned, and it never travels. `consent.json` is deliberately
    /// not in here: operator-owned versus runtime-owned is the line this
    /// directory draws.
    pub fn state(&self) -> PathBuf {
        self.meta().join(project::STATE_DIR)
    }

    pub fn gsr_pending(&self) -> PathBuf {
        self.state().join(drt_config::gsr::PENDING_DIR)
    }

    pub fn gsr_decided(&self) -> PathBuf {
        self.state().join(drt_config::gsr::DECIDED_DIR)
    }

    /// The pinned binary, if this root carries one. `drt` on `PATH` is the
    /// other way to run, and the pin check compares against whichever
    /// binary is actually executing.
    pub fn binary(&self) -> PathBuf {
        self.meta().join("drt")
    }
}

/// The root this invocation is in, or `None`.
///
/// `--root <path>` names one explicitly and is what a systemd unit should
/// use: systemd's default working directory is `/`, so a unit relying on
/// discovery would find no root and run the no-root path without saying so.
/// Without the flag, the working directory is asked and nothing else.
pub fn discover(cwd: &Path, flag: Option<&Path>) -> Option<Root> {
    let dir = match flag {
        Some(named) => named.to_path_buf(),
        None => cwd.to_path_buf(),
    };
    let root = Root { dir };
    drt_platform::fs::is_dir(root.meta()).then_some(root)
}

/// What the nesting check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nesting {
    /// Not inside another root.
    Free,
    /// Inside one, and this root says that is fine.
    Allowed { outer: PathBuf },
    /// Inside one, and this root says to say so.
    Warn { outer: PathBuf },
    /// Inside one, and this root says not to start.
    Refused { outer: PathBuf },
}

impl Root {
    /// Is this root nested beneath another, and what does *this* root say
    /// about that?
    ///
    /// The only upward walk in the system. It stops at the first `.drt_root/`
    /// above this one: a third level changes nothing about the answer, and
    /// naming the nearest is what a reader can act on.
    pub fn nesting(&self, policy: AllowNested) -> Nesting {
        let mut cursor = self.dir.parent();
        while let Some(dir) = cursor {
            if drt_platform::fs::is_dir(dir.join(project::ROOT_DIR)) {
                let outer = dir.to_path_buf();
                return match policy {
                    AllowNested::Allow => Nesting::Allowed { outer },
                    AllowNested::Warn => Nesting::Warn { outer },
                    AllowNested::Error => Nesting::Refused { outer },
                };
            }
            cursor = dir.parent();
        }
        Nesting::Free
    }

    /// Read everything `resolve` may look at.
    ///
    /// Absence is a fact rather than an error at every step: a root with no
    /// `project.json` is the fallback path, one with no `consent.json` is a
    /// first acceptance, and a profile that will not parse is reported by
    /// name instead of taking the run down before the report exists.
    pub fn read(&self, requested: Requested, overrides: Overrides) -> (ResolveInputs, Vec<String>) {
        let mut soft = Vec::new();

        let project: Option<ProjectJson> = read_json(&self.project_json(), &mut soft);
        let consent: Option<ConsentJson> = read_json(&self.consent_json(), &mut soft);

        let profile_dir = drt_platform::fs::read_dir(self.profile_dir()).unwrap_or_default();
        let mut profiles: BTreeMap<String, RootConfig> = BTreeMap::new();
        for filename in &profile_dir {
            if let Some(config) =
                read_json::<RootConfig>(&self.profile_dir().join(filename), &mut soft)
            {
                profiles.insert(filename.clone(), config);
            }
        }

        // `dlua_dir` is whichever the *resolved* profile names, and nothing
        // is resolved yet. Reading every candidate directory named by any
        // declared profile keeps this one pass: the set is small, the read
        // is a listing, and resolution then looks in the one it picked.
        let mut dlua = Vec::new();
        for config in profiles.values() {
            if let Some(dir) = &config.dlua_dir {
                dlua.extend(drt_platform::fs::read_dir(self.dir.join(dir)).unwrap_or_default());
            }
        }
        dlua.sort();
        dlua.dedup();

        let inputs = ResolveInputs {
            root: Some(RootInputs {
                project,
                consent,
                profiles,
                profile_dir,
                dlua_dir: dlua,
                // The other candidate source: a profile with no `dlua_dir`
                // deploys from `init/`, and its entry has to exist there.
                init: drt_platform::fs::read_dir(self.init()).unwrap_or_default(),
                // The pin is a fact about the binary that is running, which
                // is not necessarily `.drt_root/drt`: `drt` on `PATH` is the
                // documented happy path. So it is this binary's own identity
                // -- see `binary_version`, which is the release tag when
                // there is one and the crate version otherwise.
                binary_version: Some(binary_version()),
            }),
            config_flag: None,
            requested,
            overrides,
            cwd: drt_platform::fs::read_dir(&self.dir).unwrap_or_default(),
        };
        (inputs, soft)
    }

    /// Write `consent.json`. The operator's file, and drt writes it on a
    /// first acceptance and on a silent narrowing — operator-owned means
    /// "never travels", not "only dollup touches it".
    pub fn write_consent(&self, consent: &ConsentJson) -> Result<(), String> {
        let text = serde_json::to_string_pretty(consent)
            .map_err(|e| format!("cannot serialize consent.json: {e}"))?;
        let path = self.consent_json();
        drt_platform::fs::create_dir_all(self.meta())
            .map_err(|e| format!("cannot create {}: {e}", self.meta().display()))?;
        drt_platform::fs::write(&path, format!("{text}\n"))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Make the runtime-owned directories. Idempotent, and called before
    /// anything writes into `state/`.
    pub fn ensure_state(&self) -> Result<(), String> {
        for dir in [self.gsr_pending(), self.gsr_decided()] {
            drt_platform::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        Ok(())
    }
}

/// Read and parse, or report. A file that is not there is `None` with
/// nothing said; a file that is there and will not parse is `None` with the
/// reason recorded, because "your root is broken" without the line is not an
/// answer anyone can act on.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path, soft: &mut Vec<String>) -> Option<T> {
    let text = drt_platform::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(e) => {
            soft.push(format!("{}: {e}", path.display()));
            None
        }
    }
}

// depth: the two values drt-config takes as arguments rather than reading

/// Now, on this process's clock.
///
/// `drt-config` compares instants and never reads one, so that a page and a
/// CLI can disagree about where a clock comes from without the shared crate
/// choosing. This is drt's answer.
pub fn now() -> Timestamp {
    Timestamp::from_unix_secs(drt_platform::clock::wall_secs().unwrap_or(0) as i64)
}

/// A fresh uuid7 from this process's clock and entropy.
///
/// An entropy failure is not papered over with a zero block: an id that is
/// not random is an id two roots could share, and `root_id` is the hinge
/// shipping turns on.
pub fn mint() -> Result<Uuid7, String> {
    let mut random = [0u8; 10];
    drt_platform::entropy::fill(&mut random)
        .map_err(|e| format!("cannot mint an id: no entropy source ({e})"))?;
    let ms = drt_platform::clock::wall_ms()
        .map_err(|_| "cannot mint an id: the clock is before the epoch".to_string())?;
    Ok(Uuid7::mint(ms, random))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pin has to be the tag on a release build, because the crate
    /// version cannot tell two candidates apart.
    #[test]
    fn a_release_tag_is_the_pin_and_loses_its_v() {
        assert_eq!(pin_string(Some("v0.5.0rc9"), "0.5.0"), "0.5.0rc9");
        assert_eq!(pin_string(Some("v0.5.0"), "0.5.0"), "0.5.0");
        // A tag written without the `v` is taken as it is, rather than
        // having its first character eaten.
        assert_eq!(pin_string(Some("0.5.0rc9"), "0.5.0"), "0.5.0rc9");
    }

    /// A development tree is unchanged, which is what makes this safe to
    /// land ahead of a release: no tag, no new behaviour.
    #[test]
    fn no_tag_is_the_crate_version_exactly_as_before() {
        assert_eq!(pin_string(None, "0.5.0"), "0.5.0");
        // `build.rs` treats a blank variable as no tag, and so must this --
        // a rehearsal that exports an empty `DRT_RELEASE_TAG` must not make
        // every root's pin compare against "".
        assert_eq!(pin_string(Some(""), "0.5.0"), "0.5.0");
        assert_eq!(pin_string(Some("   "), "0.5.0"), "0.5.0");
    }

    /// This build is not a release, so the two agree here. The test exists
    /// to fail if the fallback is ever dropped.
    #[test]
    fn this_binary_reports_its_crate_version() {
        assert_eq!(binary_version(), env!("CARGO_PKG_VERSION"));
    }
    use crate::testfs::{self, Seeded};

    #[test]
    fn a_root_is_found_in_the_working_directory_and_read() {
        let seeded: Seeded = testfs::seed(false, &[]);

        let root = discover(Path::new(testfs::DIR), None).expect("a root is here");
        assert_eq!(root, seeded.root);

        let (inputs, soft) = root.read(Requested::Default, Vec::new());
        assert!(soft.is_empty(), "{soft:?}");
        let rooted = inputs.root.as_ref().unwrap();
        assert!(rooted.project.is_some());
        assert!(
            rooted.consent.is_none(),
            "a first start has no consent file"
        );
        assert_eq!(rooted.profile_dir, ["debug.config.json"]);
        assert_eq!(rooted.dlua_dir, ["app.dlua"]);

        let out = drt_config::resolve::resolve(&inputs);
        assert_eq!(
            out.profile.as_ref().unwrap().to_string(),
            "debug (default_profile)"
        );
        assert!(out.blocker().is_none(), "{:?}", out.findings);
    }

    /// Discovery does not walk up. `cd build && drt start` inside a root is
    /// the no-root path, not that root's deployment.
    #[test]
    fn discovery_does_not_walk_up() {
        let seeded = testfs::seed(false, &[]);
        seeded.fs.add_dir("/r/build");

        assert!(discover(Path::new("/r"), None).is_some());
        assert!(
            discover(Path::new("/r/build"), None).is_none(),
            "an unclaimed subdirectory of a root is not in that root"
        );
        // `--root` is how a unit with no useful cwd names one.
        assert!(discover(Path::new("/elsewhere"), Some(Path::new("/r"))).is_some());
    }

    /// The nesting check *does* walk up, and the inner root's policy decides.
    #[test]
    fn the_nesting_check_walks_up_and_the_inner_root_governs() {
        let _seeded = testfs::seed(false, &[("/r/nested/.drt_root/project.json", "{}")]);

        let inner = Root {
            dir: PathBuf::from("/r/nested"),
        };
        let outer = PathBuf::from("/r");
        assert_eq!(
            inner.nesting(AllowNested::Warn),
            Nesting::Warn {
                outer: outer.clone()
            }
        );
        assert_eq!(
            inner.nesting(AllowNested::Error),
            Nesting::Refused {
                outer: outer.clone()
            }
        );
        assert_eq!(
            inner.nesting(AllowNested::Allow),
            Nesting::Allowed { outer }
        );

        let top = Root {
            dir: PathBuf::from("/r"),
        };
        assert_eq!(top.nesting(AllowNested::Error), Nesting::Free);
    }

    /// A profile that will not parse is reported by name, and does not take
    /// the rest of the report down with it.
    #[test]
    fn an_unparseable_profile_is_reported_rather_than_fatal() {
        let _seeded = testfs::seed(
            false,
            &[("/r/.drt_root/profile/broken.config.json", "{ not json")],
        );

        let root = discover(Path::new("/r"), None).unwrap();
        let (inputs, soft) = root.read(Requested::Default, Vec::new());
        assert_eq!(soft.len(), 1, "{soft:?}");
        assert!(soft[0].contains("broken.config.json"), "{soft:?}");

        // The declared profile still resolves.
        let out = drt_config::resolve::resolve(&inputs);
        assert_eq!(out.profile.as_ref().unwrap().value.as_str(), "debug");
    }

    #[test]
    fn consent_is_written_beside_project_json_and_reads_back() {
        let seeded = testfs::seed(false, &[]);
        let id = Seeded::root_id();
        seeded.root.write_consent(&ConsentJson::new(id)).unwrap();

        let (inputs, soft) = seeded.root.read(Requested::Default, Vec::new());
        assert!(soft.is_empty(), "{soft:?}");
        assert_eq!(
            inputs
                .root
                .as_ref()
                .unwrap()
                .consent
                .as_ref()
                .unwrap()
                .root_id,
            id
        );
    }

    #[test]
    fn the_state_directories_are_made_idempotently() {
        let seeded = testfs::seed(false, &[]);

        seeded.root.ensure_state().unwrap();
        seeded.root.ensure_state().unwrap();
        assert!(drt_platform::fs::is_dir(seeded.root.gsr_pending()));
        assert!(drt_platform::fs::is_dir(seeded.root.gsr_decided()));
        assert!(
            !drt_platform::fs::exists(seeded.root.consent_json()),
            "consent.json is not in state/, and ensure_state does not invent it"
        );
    }

    /// Minting goes through the platform, and two mints are two ids. No
    /// filesystem, so no guard.
    #[test]
    fn minting_and_reading_the_clock_go_through_the_platform() {
        let first = mint().expect("entropy");
        let second = mint().expect("entropy");
        assert_ne!(first, second, "two mints are two ids");
        assert!(now().unix_secs() > 1_700_000_000, "a plausible wall clock");
    }
}
