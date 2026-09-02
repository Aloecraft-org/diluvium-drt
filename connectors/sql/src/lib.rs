//! The sql connector: `host:sql/*` (SPEC.md §7, Capabilities.md §4).
//!
//! The config grants a **scope** — a directory — and the program names the
//! database it wants within it, so *which* database is an application
//! detail living in the application. Two calls, split so the capability
//! grammar can split with them:
//!
//! ```text
//! sql/query {db, sql, params}  -> {cols, rows}
//! sql/exec  {db, sql, params}  -> {changes, rowid}
//! ```
//!
//! The contract is `dhost_sql.c`'s, kept deliberately identical so a guest
//! cannot tell the hosts apart:
//!
//! - `db` is a **filename inside the scope, never a path**. A separator, a
//!   `.` or `..`, or an embedded NUL is refused, and a name resolving
//!   through a symlink to somewhere outside the scope is refused rather
//!   than clamped.
//! - Handles open on first use and stay cached, up to a small bound, and
//!   nothing is preallocated: a deployment that grants a scope pays for the
//!   databases its programs actually name. The cache never evicts, because
//!   closing a handle out from under the autocommit discipline would be
//!   invisible state a guest cannot reason about.
//! - `query` refuses a statement that writes, and the classification is
//!   SQLite's own (`sqlite3_stmt_readonly`, after prepare) rather than a
//!   regex over the text.
//! - `exec` exists only where the config grants `access = "readwrite"`. So
//!   a grant of `host:sql/query` against a read deployment is exactly what
//!   it says, and the family wildcard on a readwrite one is the bigger
//!   thing it says.
//! - One statement per call. **Not autocommit only** — this line used to say
//!   that and it was wrong, which cost a downstream consumer a wrong plan:
//!   handles are cached, so `BEGIN`, the writes and `COMMIT` reach the same
//!   connection across separate hostcalls and a committed transaction
//!   survives. What v1 does not have is a *handle the host holds against the
//!   guest* — the endpoint-token shape — which is why a transaction is
//!   scoped to the process rather than to anything the guest can name.
//!   [`SqlConnector::finish`] is what makes the difference visible: a
//!   transaction still open when the process ends is rolled back **by name**
//!   rather than silently by SQLite, because every write inside it was
//!   already answered `ok`.
//! - The row cap **refuses rather than truncates**: a truncated result is a
//!   silent lie, and a guest can page with LIMIT/OFFSET like anything else
//!   that reads a database.
//!
//! Replay note, as `doc/Host.md` carries it: replies are in the message
//! log, so a replay *replays* them and does not re-execute against the
//! database. The database is outside the replay boundary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Deserialize;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};

/// Databases a deployment may have open at once. A bound, not a budget.
const MAX_DBS: usize = 8;
const DEFAULT_MAX_RESULT_ROWS: usize = 1000;

#[derive(Debug, Clone, Deserialize)]
struct SqlScope {
    /// The granted directory. Programs name databases within it.
    scope: PathBuf,
    /// `read` (the default) or `readwrite`. These are the C host's two
    /// spellings (`dhost.c`), and DRT accepts exactly them: a config that
    /// loads there loads here.
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    max_result_rows: Option<usize>,
    /// Whether a database the program names but which does not exist may be
    /// created. Defaults to the write grant: a readwrite deployment creates,
    /// a read-only one does not.
    #[serde(default)]
    create: Option<bool>,
}

impl SqlScope {
    fn parse(scope: Option<&Scope>) -> Result<Self, String> {
        let Some(Scope(value)) = scope else {
            return Err("scope is required: name the directory databases live in".into());
        };
        let parsed: SqlScope = if let Some(dir) = value.as_str() {
            SqlScope {
                scope: PathBuf::from(dir),
                access: None,
                max_result_rows: None,
                create: None,
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
                    "config.connectors.sql.access must be \"read\" or \
                     \"readwrite\" (got '{other}')"
                ))
            }
        }
        Ok(parsed)
    }

    fn writable(&self) -> bool {
        self.access.as_deref() == Some("readwrite")
    }

    fn may_create(&self) -> bool {
        self.create.unwrap_or_else(|| self.writable())
    }

    fn max_result_rows(&self) -> usize {
        self.max_result_rows.unwrap_or(DEFAULT_MAX_RESULT_ROWS)
    }

    fn root(&self) -> Result<PathBuf, String> {
        std::fs::canonicalize(&self.scope).map_err(|e| {
            format!(
                "scope directory {} cannot be resolved: {e}",
                self.scope.display()
            )
        })
    }

    /// Resolve a database *name* inside the scope.
    ///
    /// A name, not a path: this is stricter than the fs connector's rule on
    /// purpose, because a database is opened rather than read, and a name
    /// that could carry a directory would make "which database" a question
    /// with two answers.
    fn resolve_db(&self, name: &str) -> Result<PathBuf, String> {
        if name.is_empty() {
            return Err("the database name is empty".into());
        }
        if name.contains('\0') {
            return Err("a database name may not contain a NUL".into());
        }
        if name.contains('/') || name.contains('\\') {
            return Err(format!(
                "'{name}' contains a path separator; name a database inside the granted \
                 scope, not a path to one"
            ));
        }
        if name == "." || name == ".." || name.starts_with("..") {
            return Err(format!("'{name}' is not a database name"));
        }
        let root = self.root()?;
        let path = root.join(name);
        // If it exists, where it *really* is decides: a symlink pointing out
        // of the scope is refused rather than clamped.
        if path.exists() {
            let resolved = std::fs::canonicalize(&path).map_err(|e| format!("'{name}': {e}"))?;
            if !resolved.starts_with(&root) {
                return Err(format!(
                    "'{name}' resolves outside the granted scope; a program names databases \
                     within what the host granted, and nothing beyond it"
                ));
            }
            return Ok(resolved);
        }
        if !self.may_create() {
            return Err(format!(
                "'{name}' does not exist and this scope does not create databases"
            ));
        }
        Ok(path)
    }
}

struct SqlScopeType;

impl ScopeType for SqlScopeType {
    fn describe(&self) -> &str {
        "a directory: \"path\", or {scope, access?: read|readwrite, max_result_rows?, create?}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        SqlScopeType::check(scope)
    }
}

impl SqlScopeType {
    fn check(scope: Option<&Scope>) -> Result<(), String> {
        SqlScope::parse(scope)?.root().map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
struct SqlArgs {
    db: String,
    sql: String,
    #[serde(default)]
    params: Option<rmpv::Value>,
}

/// msgpack → SQLite. Only the types SQLite itself has; anything else is a
/// refusal naming the parameter rather than a silent coercion.
fn to_sql(value: &rmpv::Value, index: usize) -> Result<rusqlite::types::Value, CallError> {
    use rusqlite::types::Value as V;
    Ok(match value {
        rmpv::Value::Nil => V::Null,
        rmpv::Value::Boolean(b) => V::Integer(i64::from(*b)),
        rmpv::Value::Integer(n) => n
            .as_i64()
            .map(V::Integer)
            .ok_or_else(|| CallError::new(format!("parameter {index} does not fit an i64")))?,
        rmpv::Value::F32(f) => V::Real(f64::from(*f)),
        rmpv::Value::F64(f) => V::Real(*f),
        // A Lua string is a byte string; text is the useful reading and is
        // what the C connector binds.
        rmpv::Value::String(s) => match s.as_str() {
            Some(text) => V::Text(text.to_string()),
            None => V::Blob(s.as_bytes().to_vec()),
        },
        rmpv::Value::Binary(b) => V::Blob(b.clone()),
        other => {
            return Err(CallError::new(format!(
                "parameter {index} is a {other:?}, which SQLite has no type for"
            )))
        }
    })
}

/// SQLite → msgpack, for a result row.
fn from_sql(value: rusqlite::types::ValueRef<'_>) -> rmpv::Value {
    use rusqlite::types::ValueRef as V;
    match value {
        V::Null => rmpv::Value::Nil,
        V::Integer(n) => rmpv::Value::from(n),
        V::Real(f) => rmpv::Value::from(f),
        // bin and str decode to the same Lua string, and bin is byte-exact.
        V::Text(t) => rmpv::Value::Binary(t.to_vec()),
        V::Blob(b) => rmpv::Value::Binary(b.to_vec()),
    }
}

fn params_of(args: &SqlArgs) -> Result<Vec<rusqlite::types::Value>, CallError> {
    let Some(raw) = &args.params else {
        return Ok(Vec::new());
    };
    match raw {
        rmpv::Value::Nil => Ok(Vec::new()),
        // An empty Lua table arrives as a map, not an array.
        rmpv::Value::Map(m) if m.is_empty() => Ok(Vec::new()),
        rmpv::Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| to_sql(v, i + 1))
            .collect(),
        other => Err(CallError::new(format!(
            "params is a {other:?}; it is a list of values"
        ))),
    }
}

#[derive(Default)]
pub struct SqlConnector {
    /// Opened on first use, cached, never evicted (see the module note).
    open: Mutex<HashMap<PathBuf, rusqlite::Connection>>,
}

impl SqlConnector {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_db<R>(
        &self,
        scope: &SqlScope,
        name: &str,
        f: impl FnOnce(&rusqlite::Connection) -> Result<R, CallError>,
    ) -> Result<R, CallError> {
        let path = scope.resolve_db(name).map_err(CallError::new)?;
        let mut open = self
            .open
            .lock()
            .map_err(|_| CallError::new("the sql connector's handle cache is poisoned"))?;
        if !open.contains_key(&path) {
            if open.len() >= MAX_DBS {
                return Err(CallError::new(format!(
                    "this deployment already has {MAX_DBS} databases open, which is the bound"
                )));
            }
            use rusqlite::OpenFlags;
            let flags = if scope.writable() {
                if scope.may_create() {
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                } else {
                    OpenFlags::SQLITE_OPEN_READ_WRITE
                }
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let conn = rusqlite::Connection::open_with_flags(&path, flags)
                .map_err(|e| CallError::new(format!("opening '{name}': {e}")))?;
            open.insert(path.clone(), conn);
        }
        f(open.get(&path).expect("just inserted"))
    }
}

#[async_trait::async_trait]
impl Connector for SqlConnector {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(SqlScopeType)
    }

    /// depth: teardown, and the one thing this connector can lose
    ///
    /// Handles are cached across hostcalls, so `begin` in one call and no
    /// `commit` in any later one leaves a transaction open when the process
    /// ends. SQLite rolls that back when the connection drops -- correctly,
    /// and invisibly. Every layer above has already been told `ok`.
    ///
    /// So the rollback is issued here rather than left to happen. Not because
    /// the outcome differs -- it does not -- but because "the writes are gone"
    /// should not depend on a connection teardown nobody in this repository
    /// controls, and because the only way to *name* the loss is to be the one
    /// performing it. A silent rollback and an accidental commit are both ways
    /// leaving it implicit can fail, and only one of those is recoverable.
    fn finish(&self) -> Vec<String> {
        let Ok(open) = self.open.lock() else {
            return vec!["the sql connector's handle cache is poisoned; whether \
                         a transaction was open cannot be established"
                .into()];
        };
        let mut lost = Vec::new();
        for (path, conn) in open.iter() {
            // The question SQLite answers directly: outside a transaction a
            // connection is in autocommit, inside one it is not. No parsing
            // of statements, no counter of our own to get out of step.
            if conn.is_autocommit() {
                continue;
            }
            let db = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let detail = match conn.execute_batch("ROLLBACK") {
                Ok(()) => format!(
                    "'{db}': a transaction was still open at exit and has been rolled \
                     back. Writes since `begin` are gone. Every one of them \
                     was answered `ok`"
                ),
                // Failing to roll back does not mean it committed -- the
                // connection still drops -- but this connector no longer
                // knows what happened, and saying so is the only honest
                // answer left.
                Err(e) => format!(
                    "'{db}': a transaction was still open at exit and the rollback \
                     failed ({e}). The state of writes since `begin` is \
                     unknown"
                ),
            };
            lost.push(detail);
        }
        lost
    }

    async fn call(
        &self,
        call: &str,
        args: Option<rmpv::Value>,
        scope: Option<&Scope>,
    ) -> CallResult {
        let sc = SqlScope::parse(scope).map_err(CallError::new)?;
        if !matches!(call, "sql/query" | "sql/exec") {
            return Err(CallError::new(format!(
                "the sql connector answers 'sql/query' and 'sql/exec'; '{call}' is neither"
            )));
        }
        if call == "sql/exec" && !sc.writable() {
            return Err(CallError::new(
                "'sql/exec' needs access = \"readwrite\"; this scope is read-only",
            ));
        }
        let args: SqlArgs =
            rmpv::ext::from_value(args.ok_or_else(|| {
                CallError::new(format!("{call} takes args {{db, sql, params?}}"))
            })?)
            .map_err(|e| CallError::new(format!("{call} args: {e}")))?;
        let params = params_of(&args)?;
        let max_rows = sc.max_result_rows();
        let want_query = call == "sql/query";
        let sql = args.sql.clone();
        let db = args.db.clone();

        self.with_db(&sc, &db, move |conn| {
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| CallError::new(format!("preparing: {e}")))?;
            // SQLite's own classification, after prepare — not a regex over
            // the text, which is how "SELECT ... FROM f()" gets misread.
            if want_query && !stmt.readonly() {
                return Err(CallError::new(
                    "'sql/query' is for statements that read; this one writes, which is \
                     'sql/exec' and a different grant",
                ));
            }
            let bound: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            if want_query {
                let cols: Vec<rmpv::Value> = stmt
                    .column_names()
                    .iter()
                    .map(|n| rmpv::Value::from(*n))
                    .collect();
                let ncols = cols.len();
                let mut rows_out: Vec<rmpv::Value> = Vec::new();
                let mut rows = stmt
                    .query(bound.as_slice())
                    .map_err(|e| CallError::new(format!("running: {e}")))?;
                while let Some(row) = rows
                    .next()
                    .map_err(|e| CallError::new(format!("reading: {e}")))?
                {
                    if rows_out.len() >= max_rows {
                        // Refused, not truncated: a truncated result is a
                        // silent lie. Page with LIMIT/OFFSET.
                        return Err(CallError::new(format!(
                            "the result is past the {max_rows}-row cap this scope allows; \
                             page it with LIMIT/OFFSET"
                        )));
                    }
                    let mut cells = Vec::with_capacity(ncols);
                    for i in 0..ncols {
                        cells
                            .push(from_sql(row.get_ref(i).map_err(|e| {
                                CallError::new(format!("reading column {i}: {e}"))
                            })?));
                    }
                    rows_out.push(rmpv::Value::Array(cells));
                }
                Ok(rmpv::Value::Map(vec![
                    ("cols".into(), rmpv::Value::Array(cols)),
                    ("rows".into(), rmpv::Value::Array(rows_out)),
                ]))
            } else {
                let changes = stmt
                    .execute(bound.as_slice())
                    .map_err(|e| CallError::new(format!("running: {e}")))?;
                Ok(rmpv::Value::Map(vec![
                    ("changes".into(), rmpv::Value::from(changes as u64)),
                    ("rowid".into(), rmpv::Value::from(conn.last_insert_rowid())),
                ]))
            }
        })
    }
}
