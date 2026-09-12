//! One installed filesystem, one lock, for every test in this crate that
//! needs a root on disk without a disk.
//!
//! `drt_platform::fs::install` replaces the filesystem for the **process**,
//! and cargo runs tests in threads. A per-module mutex is therefore not a
//! lock at all — two modules each holding their own let a `drt_root` test and
//! a `consent_gate` test swap the backend under one another, which is a
//! failure that appears only in the full suite and passes in isolation. So
//! the lock lives here, once, and so does the guard that puts the previous
//! backend back even when a test panics.
//!
//! ## surface block
//!
//! - Entry points: [`seed`], a root laid out in memory plus the guard that
//!   holds it installed; [`Seeded`], which restores on drop.
//! - Configurable values: [`ROOT_ID`], the id every seeded root carries, and
//!   [`DIR`], where it lives.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use drt_platform::fs::{Backend, MemFs};

use crate::drt_root::Root;

/// The working directory a seeded root lives at.
pub const DIR: &str = "/r";
/// The `root_id` every seeded root carries, so a test comparing against one
/// does not have to restate it.
pub const ROOT_ID: &str = "0192f0c1-8000-7000-8000-00000000abcd";

static INSTALLED: Mutex<()> = Mutex::new(());

/// An installed in-memory filesystem and the root inside it. Hold it for the
/// body of the test; dropping it restores whatever was installed before.
pub struct Seeded {
    _exclusive: MutexGuard<'static, ()>,
    previous: Option<Arc<dyn Backend>>,
    pub fs: Arc<MemFs>,
    pub root: Root,
}

impl Drop for Seeded {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => {
                drt_platform::fs::install(previous);
            }
            None => {
                drt_platform::fs::uninstall();
            }
        }
    }
}

/// A root at [`DIR`] with a declared ceiling, one profile, and an entry,
/// plus whatever `extra` files the test wants on top.
///
/// `bare` skips the descriptor and the profile, for tests about a root that
/// has only the directory.
pub fn seed(bare: bool, extra: &[(&str, &str)]) -> Seeded {
    let exclusive = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
    let fs = Arc::new(MemFs::new());
    fs.add_dir(format!("{DIR}/.drt_root"));
    if !bare {
        fs.add_file(
            format!("{DIR}/.drt_root/project.json"),
            format!(
                r#"{{"root_id":"{ROOT_ID}",
                    "project_name":"my_drt_project","project_version":"0.0.0",
                    "caps":[{{"effect":"grant","capability":"host:fs/*"}}],
                    "default_profile":"debug","profiles":["debug.config.json"]}}"#
            ),
        );
        fs.add_file(
            format!("{DIR}/.drt_root/profile/debug.config.json"),
            r#"{"dlua_dir":"dlua/","entry":"app.dlua"}"#,
        );
        fs.add_file(format!("{DIR}/dlua/app.dlua"), "print('hi')\n");
    }
    for (path, body) in extra {
        fs.add_file(path, *body);
    }
    fs.set_cwd(DIR);
    let previous = drt_platform::fs::install(fs.clone());
    Seeded {
        _exclusive: exclusive,
        previous,
        fs,
        root: Root {
            dir: PathBuf::from(DIR),
        },
    }
}

impl Seeded {
    /// The root's id, parsed.
    pub fn root_id() -> drt_config::id::Uuid7 {
        drt_config::id::Uuid7::parse(ROOT_ID).expect("a valid uuid7")
    }

    /// `consent.json` as `start` would read it back.
    pub fn consent(&self) -> Option<drt_config::consent::ConsentJson> {
        let text = drt_platform::fs::read_to_string(self.root.consent_json()).ok()?;
        Some(serde_json::from_str(&text).expect("what was written parses"))
    }
}
