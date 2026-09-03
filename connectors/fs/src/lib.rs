//! The fs connector: `host:fs/*` (SPEC.md §7).
//!
//! The scope is a **place** — one directory the host grants — and the
//! program names its files *inside* it. That split is the whole of
//! Capabilities.md §2: config carries the directory because that is a
//! machine fact, and the program carries the filename because that is an
//! application detail. A path that names or resolves outside the scope is
//! refused, symlinks included, because a jail that only checks the string
//! it was handed is not a jail.
//!
//! `access` decides whether the writing verbs are wired at all, and it
//! defaults to `read`: a connector that silently granted writes because
//! nobody said otherwise would be the wrong default in the one place it
//! matters.
//!
//! Reads and writes both refuse past `max_bytes`, host-side. A guest cannot
//! bound its own file sizes and the instruction budget does not reach the
//! filesystem, so this is the only bound there is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use drt_platform::fs::Backend;

const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// What the host granted: a directory, how much of it, and how large a file
/// may be. Field names match discofetch's `cap6.host.lua` so a deployment
/// config ports across unchanged.
#[derive(Debug, Clone, Deserialize)]
struct FsScope {
    /// The granted directory. Programs name files within it.
    scope: PathBuf,
    /// `read` (the default) or `readwrite`. These are the C host's two
    /// spellings (`dhost.c`), and DRT accepts exactly them: a config that
    /// loads there loads here.
    #[serde(default)]
    access: Option<String>,
    /// Cap on a single file, both directions.
    #[serde(default)]
    max_bytes: Option<u64>,
}

impl FsScope {
    fn parse(scope: Option<&Scope>) -> Result<Self, String> {
        let Some(Scope(value)) = scope else {
            return Err("scope is required: name the directory this connector may use".into());
        };
        // A bare string is the ergonomic form; the map form carries options.
        let parsed: FsScope = if let Some(dir) = value.as_str() {
            FsScope {
                scope: PathBuf::from(dir),
                access: None,
                max_bytes: None,
            }
        } else {
            rmpv::ext::from_value(value.clone())
                .map_err(|e| format!("scope does not parse: {e}"))?
        };
        if parsed.scope.as_os_str().is_empty() {
            return Err("scope names no directory".into());
        }
        match parsed.access.as_deref() {
            None | Some("read") | Some("readwrite") => {}
            Some(other) => {
                return Err(format!(
                    "config.connectors.fs.access must be \"read\" or \
                     \"readwrite\" (got '{other}')"
                ))
            }
        }
        Ok(parsed)
    }

    fn writable(&self) -> bool {
        self.access.as_deref() == Some("readwrite")
    }

    fn max_bytes(&self) -> u64 {
        self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES)
    }

    /// The granted directory, resolved. Checked at startup so a scope
    /// naming a directory that is not there fails by name at boot rather
    /// than as a puzzling error on first call.
    ///
    /// Resolved through the backend, not `std::fs`: the jail is the same
    /// jail over a disk and over a page's memory filesystem, and the
    /// backend is the only thing that differs (doc/Wasm.md §4.2).
    fn root(&self, fs: &dyn Backend) -> Result<PathBuf, String> {
        fs.canonicalize(&self.scope).map_err(|e| {
            format!(
                "scope directory {} cannot be resolved: {e}",
                self.scope.display()
            )
        })
    }

    /// Resolve a guest-supplied path inside the scope, or refuse.
    ///
    /// `must_exist` is false for writes, where the file is legitimately not
    /// there yet — then the *parent* is resolved instead, so a symlinked
    /// parent pointing out of the jail is still caught.
    fn resolve(&self, fs: &dyn Backend, rel: &str, must_exist: bool) -> Result<PathBuf, String> {
        let root = self.root(fs)?;
        let rel_path = Path::new(rel);
        if names_a_root(rel_path) {
            return Err(format!(
                "'{rel}' is absolute; name a path inside the granted scope"
            ));
        }
        if rel.is_empty() {
            return Err("the path is empty".into());
        }
        // Lexically first: an escape reads the same whether or not the
        // target exists, which is a clearer message and one less thing a
        // probe can learn from the difference.
        if !lexically_within(&root, rel_path) {
            return Err(outside(rel));
        }
        let joined = root.join(rel_path);
        let resolved = if must_exist {
            fs.canonicalize(&joined)
                .map_err(|e| format!("'{rel}': {e}"))?
        } else {
            let parent = joined
                .parent()
                .ok_or_else(|| format!("'{rel}' has no parent directory"))?;
            let parent = fs
                .canonicalize(parent)
                .map_err(|e| format!("'{rel}': the containing directory {e}"))?;
            let name = joined
                .file_name()
                .ok_or_else(|| format!("'{rel}' names no file"))?;
            parent.join(name)
        };
        // And again on what the filesystem actually resolved to, which is
        // where a symlink pointing out of the jail is caught.
        if !resolved.starts_with(&root) {
            return Err(outside(rel));
        }
        Ok(resolved)
    }
}

/// Does the path start at a root? `has_root` rather than `is_absolute`:
/// on `wasm32-unknown-unknown` std counts `/etc/hosts` as not absolute (it
/// is neither unix nor wasi there), and the jail's answer must not depend
/// on which target it is asked on. A prefix (`C:`) counts too.
fn names_a_root(path: &Path) -> bool {
    path.has_root() || path.is_absolute()
}

fn outside(rel: &str) -> String {
    format!(
        "'{rel}' resolves outside the granted scope; a program names files within what the \
         host granted, and nothing beyond it"
    )
}

/// Would joining `rel` onto `root` stay under it, folding `.` and `..`
/// without asking the filesystem? A `..` that would climb above the root is
/// what this catches, and it catches it whether or not the path exists.
fn lexically_within(root: &Path, rel: &Path) -> bool {
    use std::path::Component;
    let mut depth = 0i32;
    for part in rel.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::Normal(_) => depth += 1,
            // A root or prefix component inside a supposedly relative path.
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    let _ = root;
    true
}

struct FsScopeType {
    fs: Arc<dyn Backend>,
}

impl ScopeType for FsScopeType {
    fn describe(&self) -> &str {
        "a directory: \"path\", or {scope, access?: read|readwrite, max_bytes?}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        // Resolving the directory here is the point: an unreadable or
        // missing scope is a startup refusal, by name.
        FsScope::parse(scope)?.root(&*self.fs).map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
struct PathArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    /// Bytes to write. A Lua string arrives as msgpack str or bin; both are
    /// the same byte sequence, and neither is required to be UTF-8.
    #[serde(default)]
    data: Option<rmpv::Value>,
    #[serde(default)]
    append: bool,
}

fn bytes_of(value: &rmpv::Value) -> Option<Vec<u8>> {
    match value {
        rmpv::Value::String(s) => Some(s.as_bytes().to_vec()),
        rmpv::Value::Binary(b) => Some(b.clone()),
        _ => None,
    }
}

pub struct FsConnector {
    fs: Arc<dyn Backend>,
}

impl FsConnector {
    /// Over the process's filesystem (`drt_platform::fs::host`): the disk
    /// natively and under wasmtime, the page's memory filesystem in a
    /// browser.
    pub fn new() -> Self {
        FsConnector {
            fs: drt_platform::fs::host(),
        }
    }

    /// Over a backend of the caller's choosing -- a test's, or a page's.
    pub fn with_backend(fs: Arc<dyn Backend>) -> Self {
        FsConnector { fs }
    }
}

impl Default for FsConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Connector for FsConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(FsScopeType {
            fs: self.fs.clone(),
        })
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let sc = FsScope::parse(scope).map_err(CallError::new)?;
        let need_args = |what: &str| -> Result<rmpv::Value, CallError> {
            args.clone()
                .ok_or_else(|| CallError::new(format!("{call} takes args {what}")))
        };
        let writing = matches!(call, "fs/write" | "fs/remove");
        if writing && !sc.writable() {
            return Err(CallError::new(format!(
                "'{call}' needs access = \"readwrite\"; this scope is read-only"
            )));
        }

        match call {
            "fs/read" => {
                let a: PathArgs = rmpv::ext::from_value(need_args("{path}")?)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let path = sc
                    .resolve(&*self.fs, &a.path, true)
                    .map_err(CallError::new)?;
                let meta = self
                    .fs
                    .metadata(&path)
                    .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                if meta.len > sc.max_bytes() {
                    return Err(CallError::new(format!(
                        "'{}' is {} bytes, past the {}-byte cap this scope allows",
                        a.path,
                        meta.len,
                        sc.max_bytes()
                    )));
                }
                let bytes = self
                    .fs
                    .read(&path)
                    .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                // msgpack bin and str decode identically in the guest, and
                // bin is byte-exact where a str would force UTF-8.
                Ok(rmpv::Value::Binary(bytes))
            }
            "fs/write" => {
                let a: WriteArgs = rmpv::ext::from_value(need_args("{path, data, append?}")?)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let data = a
                    .data
                    .as_ref()
                    .and_then(bytes_of)
                    .ok_or_else(|| CallError::new("fs/write needs data (a string)"))?;
                let path = sc
                    .resolve(&*self.fs, &a.path, false)
                    .map_err(CallError::new)?;
                let existing = if a.append {
                    self.fs.metadata(&path).map(|m| m.len).unwrap_or(0)
                } else {
                    0
                };
                if existing + data.len() as u64 > sc.max_bytes() {
                    return Err(CallError::new(format!(
                        "writing '{}' would reach {} bytes, past the {}-byte cap this scope \
                         allows",
                        a.path,
                        existing + data.len() as u64,
                        sc.max_bytes()
                    )));
                }
                self.fs
                    .write(&path, &data, a.append)
                    .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                Ok(rmpv::Value::Nil)
            }
            "fs/list" => {
                let rel = match args.as_ref().and_then(|v| {
                    v.as_map()
                        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("path")))
                        .and_then(|(_, v)| v.as_str())
                }) {
                    Some(p) => p.to_string(),
                    None => ".".to_string(),
                };
                let dir = sc.resolve(&*self.fs, &rel, true).map_err(CallError::new)?;
                let names = self
                    .fs
                    .read_dir(&dir)
                    .map_err(|e| CallError::new(format!("'{rel}': {e}")))?;
                Ok(rmpv::Value::Array(
                    names.into_iter().map(rmpv::Value::from).collect(),
                ))
            }
            "fs/remove" => {
                let a: PathArgs = rmpv::ext::from_value(need_args("{path}")?)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let path = sc
                    .resolve(&*self.fs, &a.path, true)
                    .map_err(CallError::new)?;
                self.fs
                    .remove_file(&path)
                    .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                Ok(rmpv::Value::Nil)
            }
            other => Err(CallError::new(format!(
                "the fs connector answers 'fs/read', 'fs/write', 'fs/list' and 'fs/remove'; \
                 '{other}' is none of them"
            ))),
        }
    }
}
