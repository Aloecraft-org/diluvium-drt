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
//! defaults to `readonly`: a connector that silently granted writes because
//! nobody said otherwise would be the wrong default in the one place it
//! matters.
//!
//! Reads and writes both refuse past `max_bytes`, host-side. A guest cannot
//! bound its own file sizes and the instruction budget does not reach the
//! filesystem, so this is the only bound there is.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};

const DEFAULT_MAX_BYTES: u64 = 1024 * 1024;

/// What the host granted: a directory, how much of it, and how large a file
/// may be. Field names match discofetch's `cap6.host.lua` so a deployment
/// config ports across unchanged.
#[derive(Debug, Clone, Deserialize)]
struct FsScope {
    /// The granted directory. Programs name files within it.
    scope: PathBuf,
    /// `readonly` (the default) or `readwrite`.
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
            None | Some("readonly") | Some("readwrite") => {}
            Some(other) => {
                return Err(format!(
                    "access is '{other}'; it is 'readonly' (the default) or 'readwrite'"
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
    fn root(&self) -> Result<PathBuf, String> {
        std::fs::canonicalize(&self.scope).map_err(|e| {
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
    fn resolve(&self, rel: &str, must_exist: bool) -> Result<PathBuf, String> {
        let root = self.root()?;
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(format!(
                "'{rel}' is absolute; name a path inside the granted scope"
            ));
        }
        if rel.is_empty() {
            return Err("the path is empty".into());
        }
        let joined = root.join(rel_path);
        let resolved = if must_exist {
            std::fs::canonicalize(&joined).map_err(|e| format!("'{rel}': {e}"))?
        } else {
            let parent = joined
                .parent()
                .ok_or_else(|| format!("'{rel}' has no parent directory"))?;
            let parent = std::fs::canonicalize(parent)
                .map_err(|e| format!("'{rel}': the containing directory {e}"))?;
            let name = joined
                .file_name()
                .ok_or_else(|| format!("'{rel}' names no file"))?;
            parent.join(name)
        };
        // The check is on the *resolved* path, so `..` and symlinks are
        // both caught by the same rule rather than by two special cases.
        if !resolved.starts_with(&root) {
            return Err(format!(
                "'{rel}' resolves outside the granted scope; a program names files within \
                 what the host granted, and nothing beyond it"
            ));
        }
        Ok(resolved)
    }
}

struct FsScopeType;

impl ScopeType for FsScopeType {
    fn describe(&self) -> &str {
        "a directory: \"path\", or {scope, access?: readonly|readwrite, max_bytes?}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        // Resolving the directory here is the point: an unreadable or
        // missing scope is a startup refusal, by name.
        FsScope::parse(scope)?.root().map(|_| ())
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

pub struct FsConnector;

impl FsConnector {
    pub fn new() -> Self {
        FsConnector
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
        Box::new(FsScopeType)
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
                let path = sc.resolve(&a.path, true).map_err(CallError::new)?;
                let meta = std::fs::metadata(&path)
                    .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                if meta.len() > sc.max_bytes() {
                    return Err(CallError::new(format!(
                        "'{}' is {} bytes, past the {}-byte cap this scope allows",
                        a.path,
                        meta.len(),
                        sc.max_bytes()
                    )));
                }
                let bytes = std::fs::read(&path)
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
                let path = sc.resolve(&a.path, false).map_err(CallError::new)?;
                let existing = if a.append {
                    std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
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
                if a.append {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                    f.write_all(&data)
                        .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                } else {
                    std::fs::write(&path, &data)
                        .map_err(|e| CallError::new(format!("'{}': {e}", a.path)))?;
                }
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
                let dir = sc.resolve(&rel, true).map_err(CallError::new)?;
                let mut names: Vec<rmpv::Value> = Vec::new();
                let entries =
                    std::fs::read_dir(&dir).map_err(|e| CallError::new(format!("'{rel}': {e}")))?;
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        names.push(rmpv::Value::from(name));
                    }
                }
                names.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                Ok(rmpv::Value::Array(names))
            }
            "fs/remove" => {
                let a: PathArgs = rmpv::ext::from_value(need_args("{path}")?)
                    .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
                let path = sc.resolve(&a.path, true).map_err(CallError::new)?;
                std::fs::remove_file(&path)
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
