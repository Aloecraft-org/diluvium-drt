//! A terminal's filesystem: the memory one a page seeds, or one the page
//! hands in under `fs` (cdn/fs.mjs makes one over an aloelite volume).
//!
//! ## surface block
//!
//! - [`JsFs::new`]: a [`PageFs`] over a page object, checked to answer
//!   [`CALLS`] before any command runs. `js::backings` makes one per
//!   terminal (and so a working directory per terminal, as a process has
//!   its own); terminals handed the same object share every file in it.
//!   The other `PageFs` is `MemFs`, the default (term.rs).
//! - [`CALLS`]: what the object is asked, every one synchronous, every
//!   path absolute and folded:
//!   - `stat(path)` -> `null` when absent, else `{ dir, size }`.
//!   - `read(path)` -> bytes.
//!   - `write(path, bytes, append)`: create, truncate, or append. The
//!     parent must exist.
//!   - `list(path)` -> the names in a directory, sorted.
//!   - `remove(path)`: a file.
//!   - `mkdirs(path)`: the directory and every one above it.
//!   - `removeAll(path)`: the directory and everything under it; absent
//!     is success.
//! - [`ERROR_KINDS`]: a thrown value's `code`, as the io error the jail
//!   and the loader read. Any other throw is `Other`, with its message.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use js_sys::{Array, Function, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};

use drt_platform::fs::{Backend, Metadata};

use crate::js::Local;
use crate::term::PageFs;

/// The calls a page filesystem answers (the surface block says what each
/// takes and returns).
pub const CALLS: [&str; 7] = [
    "stat",
    "read",
    "write",
    "list",
    "remove",
    "mkdirs",
    "removeAll",
];

/// A thrown `code`, as the io error it is. aloelite's error names, which
/// are what cdn/fs.mjs lets through, and what another object may use.
pub const ERROR_KINDS: [(&str, io::ErrorKind); 4] = [
    ("not_found", io::ErrorKind::NotFound),
    ("not_a_container", io::ErrorKind::NotADirectory),
    ("not_an_entry", io::ErrorKind::IsADirectory),
    ("already_exists", io::ErrorKind::AlreadyExists),
];

// depth: the page object behind the Backend trait

/// A filesystem the page answers: [`CALLS`] on one object.
pub struct JsFs {
    object: Local<JsValue>,
    cwd: Mutex<PathBuf>,
}

impl JsFs {
    /// Over `object`, which must have every call in [`CALLS`]. The working
    /// directory starts at `/`.
    pub fn new(object: JsValue) -> Result<JsFs, String> {
        for name in CALLS {
            let f = Reflect::get(&object, &JsValue::from_str(name)).unwrap_or(JsValue::UNDEFINED);
            if !f.is_function() {
                return Err(format!("it has no '{name}' function"));
            }
        }
        Ok(JsFs {
            object: Local(object),
            cwd: Mutex::new(PathBuf::from("/")),
        })
    }

    /// `path` against the working directory, with `.` and `..` folded.
    /// There are no symlinks to follow: a page filesystem has none drt
    /// can see, so this is `canonicalize` once the path is known to exist.
    fn absolute(&self, path: &Path) -> PathBuf {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .join(path)
        };
        let mut out = PathBuf::from("/");
        for part in joined.components() {
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

    fn invoke(&self, name: &str, path: &Path, rest: &[JsValue]) -> io::Result<JsValue> {
        let f: Function = Reflect::get(&self.object.0, &JsValue::from_str(name))
            .ok()
            .and_then(|f| f.dyn_into().ok())
            .ok_or_else(|| io::Error::other(format!("the fs backing lost its '{name}'")))?;
        let args = Array::new();
        args.push(&JsValue::from_str(&path.display().to_string()));
        for value in rest {
            args.push(value);
        }
        f.apply(&self.object.0, &args).map_err(error_of)
    }

    fn stat(&self, abs: &Path) -> io::Result<Option<Metadata>> {
        let info = self.invoke("stat", abs, &[])?;
        if info.is_null() || info.is_undefined() {
            return Ok(None);
        }
        let is_dir = Reflect::get(&info, &JsValue::from_str("dir"))
            .map(|v| v.is_truthy())
            .unwrap_or(false);
        let len = Reflect::get(&info, &JsValue::from_str("size"))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as u64;
        Ok(Some(Metadata { len, is_dir }))
    }

    fn walk(&self, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for name in self.read_dir(dir)? {
            let path = dir.join(name);
            match self.stat(&path)? {
                Some(m) if m.is_dir => self.walk(&path, out)?,
                Some(_) => out.push(path),
                None => {}
            }
        }
        Ok(())
    }
}

/// A thrown value as an io error: its `code` by [`ERROR_KINDS`], its
/// message as the text.
fn error_of(thrown: JsValue) -> io::Error {
    let field = |name: &str| {
        Reflect::get(&thrown, &JsValue::from_str(name))
            .ok()
            .and_then(|v| v.as_string())
    };
    let message = field("message")
        .or_else(|| thrown.as_string())
        .unwrap_or_else(|| "the fs backing threw".to_string());
    let kind = field("code")
        .and_then(|code| {
            ERROR_KINDS
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, k)| *k)
        })
        .unwrap_or(io::ErrorKind::Other);
    io::Error::new(kind, message)
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("No such file or directory: {}", path.display()),
    )
}

impl Backend for JsFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let abs = self.absolute(path);
        match self.stat(&abs)? {
            Some(_) => Ok(abs),
            None => Err(not_found(path)),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.stat(&self.absolute(path))?
            .ok_or_else(|| not_found(path))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let bytes = self.invoke("read", &self.absolute(path), &[])?;
        let bytes: Uint8Array = bytes
            .dyn_into()
            .map_err(|_| io::Error::other("the fs backing's read answered no bytes"))?;
        Ok(bytes.to_vec())
    }

    fn write(&self, path: &Path, data: &[u8], append: bool) -> io::Result<()> {
        self.invoke(
            "write",
            &self.absolute(path),
            &[Uint8Array::from(data).into(), JsValue::from_bool(append)],
        )
        .map(|_| ())
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        let names = self.invoke("list", &self.absolute(path), &[])?;
        let names: Array = names
            .dyn_into()
            .map_err(|_| io::Error::other("the fs backing's list answered no array"))?;
        let mut out: Vec<String> = names.iter().filter_map(|n| n.as_string()).collect();
        out.sort();
        Ok(out)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.invoke("remove", &self.absolute(path), &[]).map(|_| ())
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.invoke("mkdirs", &self.absolute(path), &[]).map(|_| ())
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.invoke("removeAll", &self.absolute(path), &[])
            .map(|_| ())
    }
}

impl PageFs for JsFs {
    fn put_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let abs = self.absolute(path);
        if let Some(parent) = abs.parent() {
            self.create_dir_all(parent)?;
        }
        self.write(&abs, bytes, false)
    }

    fn put_dir(&self, path: &Path) -> io::Result<()> {
        self.create_dir_all(path)
    }

    fn change_dir(&self, path: &Path) -> io::Result<()> {
        let abs = self.absolute(path);
        self.create_dir_all(&abs)?;
        *self.cwd.lock().unwrap_or_else(|e| e.into_inner()) = abs;
        Ok(())
    }

    fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn file_paths(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        self.walk(Path::new("/"), &mut out)?;
        Ok(out)
    }
}
