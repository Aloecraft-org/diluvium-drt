//! The filesystem behind the `fs` connector and the program loader.
//!
//! The connector's jail -- resolve inside a granted directory, refuse what
//! resolves outside it, symlinks included -- is the part that must not
//! fork between targets, so it is written once against [`Backend`] and
//! the backend is the only thing that changes: [`StdFs`] natively and
//! under wasmtime (where `std::fs` is real, over preopens), [`MemFs`] in a
//! page, where `std::fs` compiles and every call answers `Unsupported`.
//!
//! [`host`] is the backend this process reads programs and configs
//! through and wires the fs connector to by default. It is `StdFs`
//! wherever there is a filesystem and an empty `MemFs` in a page until the
//! page [`install`]s one it has seeded.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// What a file is, as much of it as the connector asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub len: u64,
    pub is_dir: bool,
}

impl Metadata {
    pub fn is_file(&self) -> bool {
        !self.is_dir
    }
}

/// The operations the fs connector, the loader, and a root's own layout
/// need, and no more.
///
/// It was six until a root had to be *written* rather than only read:
/// `dollup init`, `drt deploy`, `drt rm` and the GSR directories all make
/// and remove directory trees, and a comment promising six while the list
/// grew would be the kind of lie this file's style exists to prevent. The
/// rule is unchanged — nothing goes in here that one of those three callers
/// does not need.
pub trait Backend: Send + Sync {
    /// The path with `.`/`..` folded and symlinks followed, or an error
    /// when it does not exist -- `std::fs::canonicalize`'s contract.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn metadata(&self, path: &Path) -> io::Result<Metadata>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Create or truncate, or append when `append`. The parent directory
    /// must exist, as it must for `std::fs::write`.
    fn write(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()>;
    /// The names in a directory, sorted.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Make a directory and every directory above it. Already existing is
    /// success, as `std::fs::create_dir_all` has it: a root's layout is
    /// created idempotently and every caller would otherwise carry the same
    /// "unless it is already there" branch.
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Remove a directory and everything under it.
    ///
    /// The one destructive operation here, and the one `drt rm` is. A
    /// missing directory is success for the same reason as above: `rm` of a
    /// deployment that is not deployed has already achieved what it was
    /// asked for.
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
}

// depth: the std backend, which is the trait with the names filled in

/// `std::fs`. The real thing natively and under wasmtime.
pub struct StdFs;

impl Backend for StdFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let m = std::fs::metadata(path)?;
        Ok(Metadata {
            len: m.len(),
            is_dir: m.is_dir(),
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()> {
        if append {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            f.write_all(data)
        } else {
            std::fs::write(path, data)
        }
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            if let Some(name) = entry?.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

// depth: the in-memory backend

/// A filesystem in a map: what a page has instead of a disk.
///
/// Absolute paths under a root of `/`, a working directory relative paths
/// resolve against (`/` unless set), directories that exist because they
/// were made or because a file was put inside them. No symlinks, so
/// `canonicalize` is lexical and exact. Files a page seeds go in through
/// [`MemFs::add_file`]; what a program wrote comes out through
/// [`MemFs::files`].
pub struct MemFs {
    state: Mutex<MemState>,
}

struct MemState {
    cwd: PathBuf,
    dirs: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

impl MemFs {
    pub fn new() -> Self {
        let mut dirs = BTreeSet::new();
        dirs.insert(PathBuf::from("/"));
        MemFs {
            state: Mutex::new(MemState {
                cwd: PathBuf::from("/"),
                dirs,
                files: BTreeMap::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Make a directory and every directory above it.
    pub fn add_dir(&self, path: impl AsRef<Path>) {
        let mut st = self.lock();
        let abs = st.absolute(path.as_ref());
        st.mkdir_p(&abs);
    }

    /// Put a file in, making the directories above it.
    pub fn add_file(&self, path: impl AsRef<Path>, bytes: impl Into<Vec<u8>>) {
        let mut st = self.lock();
        let abs = st.absolute(path.as_ref());
        if let Some(parent) = abs.parent() {
            let parent = parent.to_path_buf();
            st.mkdir_p(&parent);
        }
        st.files.insert(abs, bytes.into());
    }

    /// The directory relative paths resolve against.
    pub fn set_cwd(&self, path: impl AsRef<Path>) {
        let mut st = self.lock();
        let abs = st.absolute(path.as_ref());
        st.mkdir_p(&abs);
        st.cwd = abs;
    }

    pub fn cwd(&self) -> PathBuf {
        self.lock().cwd.clone()
    }

    /// Every file, by absolute path, in path order.
    pub fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.lock()
            .files
            .iter()
            .map(|(p, b)| (p.clone(), b.clone()))
            .collect()
    }
}

impl MemState {
    /// `path` made absolute against the working directory and folded.
    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.cwd.join(path))
        }
    }

    fn mkdir_p(&mut self, abs: &Path) {
        let mut cur = PathBuf::from("/");
        for part in abs.components() {
            if let Component::Normal(name) = part {
                cur.push(name);
                self.dirs.insert(cur.clone());
            }
        }
    }

    fn is_dir(&self, abs: &Path) -> bool {
        self.dirs.contains(abs)
    }
}

/// Fold `.` and `..` lexically. `..` above the root stays at the root,
/// which is what every Unix does with it.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("No such file or directory: {}", path.display()),
    )
}

impl Backend for MemFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let st = self.lock();
        let abs = st.absolute(path);
        if st.is_dir(&abs) || st.files.contains_key(&abs) {
            Ok(abs)
        } else {
            Err(not_found(path))
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let st = self.lock();
        let abs = st.absolute(path);
        if st.is_dir(&abs) {
            return Ok(Metadata {
                len: 0,
                is_dir: true,
            });
        }
        match st.files.get(&abs) {
            Some(bytes) => Ok(Metadata {
                len: bytes.len() as u64,
                is_dir: false,
            }),
            None => Err(not_found(path)),
        }
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let st = self.lock();
        let abs = st.absolute(path);
        if st.is_dir(&abs) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("Is a directory: {}", path.display()),
            ));
        }
        st.files.get(&abs).cloned().ok_or_else(|| not_found(path))
    }

    fn write(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()> {
        let mut st = self.lock();
        let abs = st.absolute(path);
        if st.is_dir(&abs) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("Is a directory: {}", path.display()),
            ));
        }
        let parent = abs.parent().map(Path::to_path_buf).unwrap_or_default();
        if !st.is_dir(&parent) {
            return Err(not_found(&parent));
        }
        if append {
            st.files.entry(abs).or_default().extend_from_slice(data);
        } else {
            st.files.insert(abs, data.to_vec());
        }
        Ok(())
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let st = self.lock();
        let abs = st.absolute(path);
        if !st.is_dir(&abs) {
            return Err(if st.files.contains_key(&abs) {
                io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("Not a directory: {}", path.display()),
                )
            } else {
                not_found(path)
            });
        }
        let mut names: Vec<String> = st
            .dirs
            .iter()
            .chain(st.files.keys())
            .filter(|p| p.parent() == Some(abs.as_path()))
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut st = self.lock();
        let abs = st.absolute(path);
        if st.is_dir(&abs) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("Is a directory: {}", path.display()),
            ));
        }
        st.files
            .remove(&abs)
            .map(|_| ())
            .ok_or_else(|| not_found(path))
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut st = self.lock();
        let abs = st.absolute(path);
        if st.files.contains_key(&abs) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Not a directory: {}", path.display()),
            ));
        }
        st.mkdir_p(&abs);
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut st = self.lock();
        let abs = st.absolute(path);
        // Everything at or beneath it. `Path::starts_with` compares whole
        // components, not bytes, which is what makes it the right test here:
        // removing `/work` must not take `/workshop` with it, and a byte
        // prefix would. Named because the byte-prefix version is the obvious
        // one to reach for and is wrong.
        let under = |candidate: &Path| candidate.starts_with(&abs);
        st.files.retain(|candidate, _| !under(candidate));
        st.dirs
            .retain(|candidate| !under(candidate) || candidate == Path::new("/"));
        Ok(())
    }
}

// depth: the process-wide backend

static INSTALLED: Mutex<Option<Arc<dyn Backend>>> = Mutex::new(None);
static DEFAULT: OnceLock<Arc<dyn Backend>> = OnceLock::new();

/// The backend this process reads through: what [`install`] set, else
/// [`StdFs`] where there is a filesystem and an empty [`MemFs`] in a page.
pub fn host() -> Arc<dyn Backend> {
    if let Some(fs) = INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return fs.clone();
    }
    DEFAULT
        .get_or_init(|| {
            #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
            {
                Arc::new(MemFs::new())
            }
            #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
            {
                Arc::new(StdFs)
            }
        })
        .clone()
}

/// Make `backend` the process's filesystem. A page calls this with the
/// `MemFs` it seeded before it runs anything; a test calls it to run a
/// loader against files that were never on disk. Returns what was
/// installed before.
pub fn install(backend: Arc<dyn Backend>) -> Option<Arc<dyn Backend>> {
    INSTALLED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(backend)
}

/// Undo [`install`]; the default backend is back.
pub fn uninstall() -> Option<Arc<dyn Backend>> {
    INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// `std::fs::read`, through [`host`].
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    host().read(path.as_ref())
}

/// `std::fs::read_to_string`, through [`host`].
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let bytes = read(path)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// `std::fs::write`, through [`host`]. Creating or truncating.
pub fn write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
    host().write(path.as_ref(), data.as_ref(), false)
}

/// `std::fs::create_dir_all`, through [`host`].
pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    host().create_dir_all(path.as_ref())
}

/// `std::fs::remove_dir_all`, through [`host`]. A missing directory is
/// success.
pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    host().remove_dir_all(path.as_ref())
}

/// `std::fs::remove_file`, through [`host`].
pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    host().remove_file(path.as_ref())
}

/// The names in a directory, sorted, through [`host`].
pub fn read_dir(path: impl AsRef<Path>) -> io::Result<Vec<String>> {
    host().read_dir(path.as_ref())
}

/// Does this path exist at all? The question a root's layout asks of every
/// one of its directories, and the one a `metadata` call answers awkwardly.
pub fn exists(path: impl AsRef<Path>) -> bool {
    host().metadata(path.as_ref()).is_ok()
}

/// Is this path a directory? `false` for a file and for nothing at all,
/// because the callers here are asking "may I descend", not "what is it".
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    host()
        .metadata(path.as_ref())
        .map(|m| m.is_dir)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removing a tree takes what is under it and nothing that merely
    /// shares a name prefix.
    #[test]
    fn removing_a_directory_tree_is_component_wise() {
        let fs = MemFs::new();
        fs.add_file("/work/a.txt", "a");
        fs.add_file("/work/deep/b.txt", "b");
        fs.add_file("/workshop/c.txt", "c");

        fs.remove_dir_all(Path::new("/work")).unwrap();

        assert!(fs.read(Path::new("/work/a.txt")).is_err());
        assert!(fs.read(Path::new("/work/deep/b.txt")).is_err());
        assert!(
            fs.read_dir(Path::new("/work")).is_err(),
            "the directory is gone too"
        );
        assert_eq!(
            fs.read(Path::new("/workshop/c.txt")).unwrap(),
            b"c",
            "a name prefix is not a parent"
        );

        // Removing what is not there is success: `rm` of an undeployed
        // deployment has already achieved what it was asked for.
        assert!(fs.remove_dir_all(Path::new("/work")).is_ok());
    }

    /// `create_dir_all` makes the whole chain and is idempotent, so a root's
    /// layout can be created without every caller branching on existence.
    #[test]
    fn creating_a_directory_chain_is_idempotent() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/r/.drt_root/state/gsr/pending"))
            .unwrap();
        fs.create_dir_all(Path::new("/r/.drt_root/state/gsr/pending"))
            .unwrap();

        assert!(fs.metadata(Path::new("/r/.drt_root/state")).unwrap().is_dir);
        fs.write(
            Path::new("/r/.drt_root/state/gsr/pending/x.json"),
            b"{}",
            false,
        )
        .unwrap();
        assert_eq!(
            fs.read_dir(Path::new("/r/.drt_root/state/gsr/pending"))
                .unwrap(),
            ["x.json"]
        );

        // A file is not a directory, and saying so beats making one.
        fs.add_file("/r/note.txt", "n");
        assert!(fs.create_dir_all(Path::new("/r/note.txt")).is_err());
    }

    #[test]
    fn a_memory_filesystem_behaves_like_a_disk_for_every_call() {
        let fs = MemFs::new();
        fs.add_file("/work/note.txt", "hello");
        fs.set_cwd("/work");

        // canonicalize: lexical, relative to the cwd, and only for what exists
        assert_eq!(
            fs.canonicalize(Path::new("note.txt")).unwrap(),
            Path::new("/work/note.txt")
        );
        assert_eq!(
            fs.canonicalize(Path::new("./../work/./note.txt")).unwrap(),
            Path::new("/work/note.txt")
        );
        assert_eq!(fs.canonicalize(Path::new(".")).unwrap(), Path::new("/work"));
        assert_eq!(
            fs.canonicalize(Path::new("missing.txt"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        // `..` above the root clamps to the root, as on every Unix
        assert_eq!(
            fs.canonicalize(Path::new("/../..")).unwrap(),
            Path::new("/")
        );

        // read, metadata
        assert_eq!(fs.read(Path::new("note.txt")).unwrap(), b"hello");
        assert_eq!(
            fs.metadata(Path::new("note.txt")).unwrap(),
            Metadata {
                len: 5,
                is_dir: false
            }
        );
        assert!(fs.metadata(Path::new("/work")).unwrap().is_dir);

        // write, append, into an existing directory only
        fs.write(Path::new("log.txt"), b"first\n", false).unwrap();
        fs.write(Path::new("log.txt"), b"second\n", true).unwrap();
        assert_eq!(
            fs.read(Path::new("/work/log.txt")).unwrap(),
            b"first\nsecond\n"
        );
        assert_eq!(
            fs.write(Path::new("nowhere/x.txt"), b"x", false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        fs.write(Path::new("log.txt"), b"third", false).unwrap();
        assert_eq!(fs.read(Path::new("log.txt")).unwrap(), b"third");

        // list, sorted, files and directories alike
        fs.add_dir("/work/sub");
        assert_eq!(
            fs.read_dir(Path::new(".")).unwrap(),
            ["log.txt", "note.txt", "sub"]
        );
        assert_eq!(
            fs.read_dir(Path::new("note.txt")).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );

        // remove
        fs.remove_file(Path::new("log.txt")).unwrap();
        assert_eq!(
            fs.remove_file(Path::new("log.txt")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs.files().len(), 1);
    }

    #[test]
    fn the_host_backend_can_be_replaced_and_put_back() {
        let mem = Arc::new(MemFs::new());
        mem.add_file("/prog.dlua", "print(1)");
        let before = install(mem.clone());
        assert_eq!(read_to_string("/prog.dlua").unwrap(), "print(1)");
        match before {
            Some(prev) => {
                install(prev);
            }
            None => {
                uninstall();
            }
        }
        assert!(read_to_string("/prog.dlua").is_err() || Path::new("/prog.dlua").exists());
    }
}
