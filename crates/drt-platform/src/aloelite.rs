//! An aloelite volume as a [`Backend`] (doc/doc_old/Aloelite.md §3): the
//! third backend beside [`super::fs::StdFs`] and [`super::fs::MemFs`], so a
//! program reading `fs/read` cannot tell a directory from a volume.
//!
//! Native and `wasm32-wasip2`, where the volume file is a real file SQLite
//! opens. A page reaches aloelite another way: crates/drt-web's `fs`
//! backing over aloelite-wasm (cdn/fs.mjs).
//!
//! ## surface block
//!
//! - [`AloeliteFs::open`]: a volume file, a volume by name (made when the
//!   file holds none of that name; the file is made when absent), the
//!   volume path its `/` is, and whether it is mounted read-write.
//! - [`MOUNT_TTL_MS`]: the lease every mount carries (ACC-3). A process
//!   that dies leaves a mount that lapses by itself, so the next run's
//!   read-write mount is admitted; a live one renews before half of it is
//!   gone.
//! - [`error_kind`]: aloelite's errors as the io errors the jail and the
//!   loader read: not_found, not_a_container, not_an_entry, already_exists
//!   and not_empty have kinds of their own; the rest are `Other`.
//!
//! A file closed cleanly is left out of WAL, so a volume written natively
//! opens under wasmtime and back (see `Drop`).
//!
//! Paths are absolute inside the mount, folded lexically: a volume has no
//! symlink the jail must follow, so `canonicalize` is folding plus a check
//! that the node exists.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use aloelite_core::ops::{self, MountOptions};
use aloelite_core::platform::Clock;
use aloelite_core::types::{Access, MountId, NodeType, VolumeId};
use aloelite_core::{Db, FsError};

use crate::fs::{Backend, Metadata};

/// A mount's lease, renewed on use.
pub const MOUNT_TTL_MS: i64 = 30_000;

/// aloelite's errors that have an io kind of their own; the rest are
/// `Other`, carrying aloelite's sentence.
pub fn error_kind(e: &FsError) -> io::ErrorKind {
    match e {
        FsError::NotFound { .. } => io::ErrorKind::NotFound,
        FsError::NotAContainer { .. } => io::ErrorKind::NotADirectory,
        FsError::NotAnEntry { .. } => io::ErrorKind::IsADirectory,
        FsError::ContainerExists { .. } | FsError::AlreadyExists { .. } => {
            io::ErrorKind::AlreadyExists
        }
        FsError::NotEmpty { .. } => io::ErrorKind::DirectoryNotEmpty,
        _ => io::ErrorKind::Other,
    }
}

/// One mounted volume.
pub struct AloeliteFs {
    state: Mutex<State>,
}

struct State {
    db: Db,
    volume: VolumeId,
    at: String,
    access: Access,
    mount: MountId,
    /// When the lease runs out, in the engine's nanoseconds.
    expires_ns: i64,
}

impl AloeliteFs {
    /// Open `file`, find or make `volume`, and mount it at `mount_point`.
    /// Everything that can be wrong is said here, by name: a file SQLite
    /// cannot open, an era the engine refuses, a mount point the volume
    /// does not have, a read-write mount another process holds.
    pub fn open(
        file: &Path,
        volume: &str,
        mount_point: &str,
        writable: bool,
    ) -> Result<AloeliteFs, String> {
        let say = |e: &dyn std::fmt::Display| format!("volume file {}: {e}", file.display());
        let conn = rusqlite::Connection::open(file).map_err(|e| say(&e))?;
        let mut db = Db::open(conn, SystemClock, OsEntropy).map_err(|e| say(&e))?;
        let found = ops::list_volumes(&mut db)
            .map_err(|e| say(&e))?
            .into_iter()
            .filter(|v| v.name.as_deref() == Some(volume))
            .max_by(|x, y| (x.created_at, &x.id.0).cmp(&(y.created_at, &y.id.0)))
            .map(|v| v.id);
        let volume_id = match found {
            Some(id) => id,
            None => {
                ops::create_volume(
                    &mut db,
                    Some(volume),
                    DEFAULT_CHUNK_SIZE,
                    None,
                    aloelite_core::crypto::EncMode::None,
                )
                .map_err(|e| say(&e))?
                .id
            }
        };
        let access = if writable { Access::Rw } else { Access::Ro };
        let mut state = State {
            db,
            volume: volume_id,
            at: mount_point.to_string(),
            access,
            mount: MountId(String::new()),
            expires_ns: 0,
        };
        state
            .remount()
            .map_err(|e| say(&format!("volume '{volume}' at '{mount_point}': {e}")))?;
        Ok(AloeliteFs {
            state: Mutex::new(state),
        })
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&mut Db, &MountId) -> aloelite_core::Result<T>,
    ) -> io::Result<T> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.lease().map_err(to_io)?;
        let State { db, mount, .. } = &mut *st;
        f(db, mount).map_err(to_io)
    }

    fn stat(&self, path: &Path) -> io::Result<Option<Metadata>> {
        let at = inside(path);
        self.with(|db, m| match ops::stat(db, m, &at) {
            Ok(info) => Ok(Some(Metadata {
                len: info.size.unwrap_or(0).max(0) as u64,
                is_dir: info.kind == NodeType::Container,
            })),
            Err(FsError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        })
    }
}

impl Drop for AloeliteFs {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        let mount = st.mount.clone();
        let _ = ops::unmount(&mut st.db, &mount);
        // aloelite opens every file in WAL where it can, and WAL is a
        // property of the file: one left in it cannot be opened under
        // wasmtime, whose SQLite has no shared memory. Leaving it in a
        // rollback journal on close keeps the file portable between targets
        // (the next open turns WAL back on); a process that dies first
        // leaves it in WAL.
        let _ = st
            .db
            .connection()
            .query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()));
    }
}

// depth: the lease, and the Backend operations over it

/// aloelite-api's default chunk size (CV-1), which a volume fixes at birth.
const DEFAULT_CHUNK_SIZE: usize = 1_048_576;

impl State {
    fn remount(&mut self) -> aloelite_core::Result<()> {
        let opts = MountOptions {
            at: &self.at,
            ttl_ms: Some(MOUNT_TTL_MS),
            access: self.access,
            ..MountOptions::default()
        };
        self.mount = ops::mount(&mut self.db, &self.volume, &opts)?;
        self.expires_ns = self.db.now_ns() + MOUNT_TTL_MS * 1_000_000;
        Ok(())
    }

    /// Renew the lease once half of it is gone; mount again when it lapsed,
    /// which a process idle past the ttl finds on its next call.
    fn lease(&mut self) -> aloelite_core::Result<()> {
        let now = self.db.now_ns();
        if now + MOUNT_TTL_MS * 500_000 < self.expires_ns {
            return Ok(());
        }
        match ops::renew_mount(&mut self.db, &self.mount, Some(MOUNT_TTL_MS)) {
            Ok(_) => {
                self.expires_ns = now + MOUNT_TTL_MS * 1_000_000;
                Ok(())
            }
            Err(FsError::MountInvalid { .. }) => self.remount(),
            Err(e) => Err(e),
        }
    }
}

fn to_io(e: FsError) -> io::Error {
    io::Error::new(error_kind(&e), e.to_string())
}

/// `path` as a mount-relative aloelite path: absolute, `.` and `..` folded.
fn inside(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    format!("/{}", parts.join("/"))
}

impl Backend for AloeliteFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        match self.stat(path)? {
            Some(_) => Ok(PathBuf::from(inside(path))),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("No such file or directory: {}", path.display()),
            )),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.stat(path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("No such file or directory: {}", path.display()),
            )
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let at = inside(path);
        self.with(|db, m| ops::read_all(db, m, &at))
    }

    fn write(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()> {
        let at = inside(path);
        let existing = self.stat(path)?;
        if existing.is_some_and(|m| m.is_dir) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("Is a directory: {}", path.display()),
            ));
        }
        self.with(|db, m| match (existing, append) {
            (None, _) => ops::create_entry(db, m, &at, Some(data)).map(|_| ()),
            (Some(_), true) => ops::append(db, m, &at, data).map(|_| ()),
            (Some(_), false) => ops::write_all(db, m, &at, data),
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let at = inside(path);
        let mut names: Vec<String> = self
            .with(|db, m| ops::list(db, m, &at))?
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        names.sort();
        Ok(names)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self.stat(path)?.is_some_and(|m| m.is_dir) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("Is a directory: {}", path.display()),
            ));
        }
        let at = inside(path);
        self.with(|db, m| ops::remove(db, m, &at))
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut here = PathBuf::from("/");
        for part in Path::new(&inside(path)).components() {
            if let Component::Normal(name) = part {
                here.push(name);
                match self.stat(&here)? {
                    Some(m) if m.is_dir => {}
                    Some(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            format!("Not a directory: {}", here.display()),
                        ))
                    }
                    None => {
                        let at = inside(&here);
                        self.with(|db, m| ops::create_container(db, m, &at).map(|_| ()))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        if self.stat(path)?.is_none() {
            return Ok(());
        }
        let at = inside(path);
        if at == "/" {
            // The mount point is the backend's `/` and stays, as MemFs keeps
            // its root.
            for name in self.read_dir(path)? {
                let child = format!("/{name}");
                self.with(|db, m| ops::remove_recursive(db, m, &child))?;
            }
            return Ok(());
        }
        self.with(|db, m| ops::remove_recursive(db, m, &at))
    }
}

// depth: the clock and entropy the engine is handed

struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }
}

/// The OS CSPRNG (`crate::entropy::fill`) as rand_core 0.10's generator.
/// The engine wants an infallible one, and nonces or keys from a failed
/// source would be worse than stopping, so a failure panics by name.
struct OsEntropy;

fn fill(buf: &mut [u8]) {
    crate::entropy::fill(buf)
        .expect("the OS entropy source failed; aloelite cannot mint ids or keys");
}

impl rand_core::TryRng for OsEntropy {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut b = [0u8; 4];
        fill(&mut b);
        Ok(u32::from_le_bytes(b))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut b = [0u8; 8];
        fill(&mut b);
        Ok(u64::from_le_bytes(b))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        fill(dst);
        Ok(())
    }
}

impl rand_core::TryCryptoRng for OsEntropy {}
