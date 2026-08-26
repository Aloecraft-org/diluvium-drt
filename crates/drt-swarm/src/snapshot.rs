//! The snapshot store (SPEC.md §8): durable agents. Snapshots survive the
//! process — it is bytes to files, and doing it in v1 is what forces the ref
//! encoding right (`refs`), because a snapshot restored on another machine
//! next week must still resolve its endpoints.
//!
//! The *cache* of hot snapshots is the swarm's (owned thing six); this store
//! is the durable layer behind it. The identity stamp is `dv_snapshot`'s
//! `host` argument: a snapshot with no stamp restores anywhere, a stamped one
//! restores only under the same string, and a store that stamps refuses a
//! snapshot without one — stamping is never advisory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where snapshots go. One trait so a deployment can swap the directory for
/// object storage without the swarm noticing.
pub trait SnapshotStore: Send + Sync {
    fn put(&self, name: &str, bytes: &[u8]) -> io::Result<()>;
    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>>;
    fn remove(&self, name: &str) -> io::Result<()>;
    fn list(&self) -> io::Result<Vec<String>>;
}

/// The v1 impl: a directory of files. Writes are atomic (temp file + rename)
/// so a crash mid-write leaves the previous snapshot, never a truncated one —
/// a corrupt snapshot is refused by `dv_restore`, but the store should not
/// manufacture corrupt snapshots in the first place.
pub struct DirectoryStore {
    dir: PathBuf,
}

impl DirectoryStore {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(DirectoryStore { dir })
    }

    /// Snapshot names come from programs (instance names travel in lifecycle
    /// requests), so they are path-checked, not trusted: one plain component,
    /// no separators, no leading dot.
    fn path_for(&self, name: &str) -> io::Result<PathBuf> {
        let ok = !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{name}' is not a snapshot name: one path component, no leading dot"),
            ));
        }
        Ok(self.dir.join(format!("{name}.dvsnap")))
    }
}

impl SnapshotStore for DirectoryStore {
    fn put(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path_for(name)?;
        let tmp = path.with_extension("dvsnap.tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)
    }

    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path_for(name)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.path_for(name)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if let Some(name) = snapshot_name(&path) {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
}

fn snapshot_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()?.strip_suffix(".dvsnap")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(dir.path()).unwrap();
        store.put("agent-1", b"parked state").unwrap();
        assert_eq!(store.get("agent-1").unwrap().unwrap(), b"parked state");
        // Durable: a fresh store over the same directory still has it.
        let reopened = DirectoryStore::open(dir.path()).unwrap();
        assert_eq!(reopened.get("agent-1").unwrap().unwrap(), b"parked state");
        assert_eq!(reopened.list().unwrap(), ["agent-1"]);
        reopened.remove("agent-1").unwrap();
        assert_eq!(reopened.get("agent-1").unwrap(), None);
        reopened.remove("agent-1").unwrap();
    }

    #[test]
    fn overwrite_replaces_whole_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(dir.path()).unwrap();
        store.put("a", b"first, and longer").unwrap();
        store.put("a", b"second").unwrap();
        assert_eq!(store.get("a").unwrap().unwrap(), b"second");
    }

    #[test]
    fn names_are_checked_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(dir.path()).unwrap();
        for bad in ["", "..", "../escape", "a/b", ".hidden", "a\\b"] {
            assert!(store.put(bad, b"x").is_err(), "accepted {bad:?}");
        }
        store.put("ok-name_1.v2", b"x").unwrap();
    }
}
