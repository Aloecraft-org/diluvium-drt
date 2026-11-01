//! `sql/*` in a page, over sql.js (issue #81, route 1): the native sql
//! connector's contract (`connectors/sql`), answered by the SQLite the
//! page hands in, so a program cannot tell which host it is on.
//!
//! ## surface block
//!
//! - [`SqlJs::new`]: over a sql.js module (what `await initSqlJs()`
//!   answers) and the terminal's filesystem. `js::backings` makes one per
//!   command when the page hands in `{ sql: SQL }`.
//! - The calls, as native: `sql/query {db, sql, params}` -> `{cols, rows}`
//!   and `sql/exec {db, sql, params}` -> `{changes, rowid}`.
//! - The scope, as native: `"dir"` or `{scope, access?, max_result_rows?,
//!   create?, journal_mode?}`, a directory of the page's filesystem.
//! - [`MAX_DBS`], [`DEFAULT_MAX_RESULT_ROWS`]: native's bounds.
//!
//! **Where a database lives.** In the page's filesystem, inside the
//! granted directory, as a SQLite file, exactly where the native connector
//! would put it: opened from its bytes, and written back after every
//! write that leaves no transaction open. So a second command in the same
//! terminal, or the page itself (`term.getFile`), sees what the first
//! wrote, and a database the page seeds is the database the program
//! reads. A transaction still open at the end of the command is rolled
//! back and said so, by name, in native's words.
//!
//! **How the native rules are kept.** `query` refuses a statement that
//! writes by SQLite's own classification: native asks
//! `sqlite3_stmt_readonly`, which sql.js does not export, so this reads
//! the same flag off SQLite's own code for the statement (`EXPLAIN`: a
//! write transaction, a checkpoint, a vacuum or a journal-mode change is
//! what clears it in `vdbeaux.c`). One statement per call, as rusqlite's
//! "Multiple statements provided". `exec` of a statement that returns rows
//! is rusqlite's "Execute returned results". Text comes back as bytes,
//! integers as integers and reals as reals, as native's `from_sql` does.
//!
//! **What differs, said here so nobody has to find it.** SQLite's own
//! error text reaches the guest as sql.js words it (a syntax error reads
//! `near "selec": syntax error` rather than rusqlite's longer form). An
//! integer parameter past 2^31 is bound as a real by sql.js, which a
//! column with integer affinity stores back as an integer and an untyped
//! one keeps as a real. A database is held in memory while open.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use js_sys::{Array, Function, Reflect, Uint8Array};
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use drt_caps::{Scope, ScopeType};
use drt_connector::{CallError, CallResult, Connector};
use drt_platform::fs::Backend;

use crate::js::Local;

/// Databases one command may have open at once: native's bound.
pub const MAX_DBS: usize = 8;
/// Rows a query may answer when the scope names no cap: native's.
pub const DEFAULT_MAX_RESULT_ROWS: usize = 1000;
/// What `journal_mode` may name, as native checks it at startup.
const JOURNAL_MODES: [&str; 6] = ["wal", "delete", "truncate", "persist", "memory", "off"];
/// The opcodes that clear a statement's read-only flag (`vdbeaux.c`,
/// `resolveP2Values`), beside `Transaction` with a non-zero P2.
const WRITING_OPCODES: [&str; 3] = ["Checkpoint", "Vacuum", "JournalMode"];

/// The connector: a sql.js module, the filesystem databases live in, and
/// the databases open so far.
pub struct SqlJs {
    module: Local<JsValue>,
    fs: Arc<dyn Backend>,
    open: Mutex<HashMap<PathBuf, Local<JsValue>>>,
}

impl SqlJs {
    pub fn new(module: JsValue, fs: Arc<dyn Backend>) -> Self {
        SqlJs {
            module: Local(module),
            fs,
            open: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl Connector for SqlJs {
    fn scope_type(&self) -> Box<dyn ScopeType> {
        Box::new(SqlJsScopeType {
            fs: Arc::clone(&self.fs),
        })
    }

    fn finish(&self) -> Vec<String> {
        let open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let mut lost = Vec::new();
        for (path, db) in open.iter() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            if autocommit(&db.0) {
                continue;
            }
            lost.push(match run(&db.0, "ROLLBACK") {
                Ok(()) => format!(
                    "'{name}': a transaction was still open at exit and has been rolled \
                     back. Writes since `begin` are gone. Every one of them \
                     was answered `ok`"
                ),
                Err(e) => format!(
                    "'{name}': a transaction was still open at exit and the rollback \
                     failed ({e}). The state of writes since `begin` is \
                     unknown"
                ),
            });
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
        let path = sc
            .resolve_db(self.fs.as_ref(), &args.db)
            .map_err(CallError::new)?;
        let db = self.db(&sc, &path, &args.db)?;
        if call == "sql/query" {
            query(&db, &args.sql, &params, sc.max_result_rows())
        } else {
            let answer = exec(&db, &args.sql, &params)?;
            // Written back once nothing is pending: a write inside an open
            // transaction is SQLite's to keep until COMMIT.
            if autocommit(&db) {
                self.persist(&path, &db)?;
            }
            Ok(answer)
        }
    }
}

// depth: opening and keeping databases

impl SqlJs {
    /// The database at `path`, opened on first use from the file's bytes,
    /// or new where the scope creates. Kept until the command ends.
    fn db(&self, sc: &SqlScope, path: &Path, name: &str) -> Result<JsValue, CallError> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(db) = open.get(path) {
            return Ok(db.0.clone());
        }
        if open.len() >= MAX_DBS {
            return Err(CallError::new(format!(
                "this deployment already has {MAX_DBS} databases open, which is the bound"
            )));
        }
        let ctor: Function = Reflect::get(&self.module.0, &JsValue::from_str("Database"))
            .ok()
            .and_then(|f| f.dyn_into().ok())
            .ok_or_else(|| CallError::new("the page's sql backing has no Database"))?;
        let bytes = self.fs.read(path).ok();
        let fresh = bytes.is_none();
        let init = match bytes {
            Some(b) => JsValue::from(Uint8Array::from(b.as_slice())),
            None => JsValue::UNDEFINED,
        };
        let db = Reflect::construct(&ctor, &Array::of1(&init))
            .map_err(|e| CallError::new(format!("opening '{name}': {}", message(&e))))?;
        if let Some(want) = sc.journal_mode.as_deref() {
            let want = want.to_ascii_lowercase();
            let got = scalar(&db, &format!("PRAGMA journal_mode = {want}"))
                .map_err(|e| CallError::new(format!("opening '{name}': journal_mode: {e}")))?
                .as_string()
                .unwrap_or_default();
            if got.to_ascii_lowercase() != want {
                return Err(CallError::new(format!(
                    "opening '{name}': the scope asks for journal_mode {want} and SQLite \
                     answered {got}; a read-only open cannot convert a database, and WAL \
                     needs a filesystem SQLite can lock"
                )));
            }
        }
        // Created where native's open creates: the file exists from here.
        if fresh {
            self.persist(path, &db)?;
        }
        open.insert(path.to_path_buf(), Local(db.clone()));
        Ok(db)
    }

    fn persist(&self, path: &Path, db: &JsValue) -> Result<(), CallError> {
        let export = method(db, "export").map_err(CallError::new)?;
        let bytes = export
            .call0(db)
            .map_err(|e| CallError::new(format!("writing back: {}", message(&e))))?;
        let bytes: Uint8Array = bytes
            .dyn_into()
            .map_err(|_| CallError::new("writing back: export answered no bytes"))?;
        self.fs
            .write(path, &bytes.to_vec(), false)
            .map_err(|e| CallError::new(format!("writing back: {e}")))
    }
}

// depth: the two calls

fn query(db: &JsValue, sql: &str, params: &Array, max_rows: usize) -> CallResult {
    let stmt = prepare(db, sql)?;
    let answer = (|| {
        if !readonly(db, &stmt)? {
            return Err(CallError::new(
                "'sql/query' is for statements that read; this one writes, which is \
                 'sql/exec' and a different grant",
            ));
        }
        bind(&stmt, params)?;
        let cols: Vec<rmpv::Value> = Array::from(&call0(&stmt, "getColumnNames")?)
            .iter()
            .map(|c| rmpv::Value::from(c.as_string().unwrap_or_default()))
            .collect();
        let options = js_sys::Object::new();
        let _ = Reflect::set(&options, &"useBigInt".into(), &JsValue::TRUE);
        let mut rows = Vec::new();
        while step(&stmt).map_err(|e| CallError::new(format!("reading: {e}")))? {
            if rows.len() >= max_rows {
                return Err(CallError::new(format!(
                    "the result is past the {max_rows}-row cap this scope allows; \
                     page it with LIMIT/OFFSET"
                )));
            }
            let get = method(&stmt, "get").map_err(CallError::new)?;
            let row = get
                .call2(&stmt, &JsValue::NULL, &options)
                .map_err(|e| CallError::new(format!("reading: {}", message(&e))))?;
            rows.push(rmpv::Value::Array(
                Array::from(&row).iter().map(|v| from_sql(&v)).collect(),
            ));
        }
        Ok(rmpv::Value::Map(vec![
            ("cols".into(), rmpv::Value::Array(cols)),
            ("rows".into(), rmpv::Value::Array(rows)),
        ]))
    })();
    free(&stmt);
    answer
}

fn exec(db: &JsValue, sql: &str, params: &Array) -> CallResult {
    let stmt = prepare(db, sql)?;
    let answer = (|| {
        bind(&stmt, params)?;
        if step(&stmt).map_err(|e| CallError::new(format!("running: {e}")))? {
            return Err(CallError::new(
                "running: Execute returned results - did you mean to call query?",
            ));
        }
        let changes = call0(db, "getRowsModified")?.as_f64().unwrap_or(0.0) as u64;
        let rowid = scalar(db, "SELECT last_insert_rowid()").map_err(CallError::new)?;
        Ok(rmpv::Value::Map(vec![
            ("changes".into(), rmpv::Value::from(changes)),
            ("rowid".into(), from_sql(&rowid)),
        ]))
    })();
    free(&stmt);
    answer
}

/// One statement, prepared. More than one is rusqlite's refusal; the
/// count is SQLite's, from preparing each in turn and running none. The
/// iterator owns what it prepares and finalizes it as it moves on, so the
/// statement run is prepared again from its own text.
fn prepare(db: &JsValue, sql: &str) -> Result<JsValue, CallError> {
    let iterate = method(db, "iterateStatements").map_err(CallError::new)?;
    let it = iterate
        .call1(db, &JsValue::from_str(sql))
        .map_err(|e| CallError::new(format!("preparing: {}", message(&e))))?;
    let next = method(&it, "next").map_err(CallError::new)?;
    let mut first: Option<String> = None;
    loop {
        let step = next
            .call0(&it)
            .map_err(|e| CallError::new(format!("preparing: {}", message(&e))))?;
        if Reflect::get(&step, &"done".into())
            .ok()
            .and_then(|d| d.as_bool())
            == Some(true)
        {
            break;
        }
        if first.is_some() {
            if let Ok(stop) = method(&it, "return") {
                let _ = stop.call0(&it);
            }
            return Err(CallError::new("preparing: Multiple statements provided"));
        }
        let stmt = Reflect::get(&step, &"value".into()).unwrap_or(JsValue::UNDEFINED);
        first = Some(call0(&stmt, "getSQL")?.as_string().unwrap_or_default());
    }
    // Nothing to run is an empty statement, as rusqlite prepares one.
    call1(
        db,
        "prepare",
        &JsValue::from_str(first.as_deref().unwrap_or("")),
    )
}

/// `sqlite3_stmt_readonly`, read off the statement's own program.
fn readonly(db: &JsValue, stmt: &JsValue) -> Result<bool, CallError> {
    let text = call0(stmt, "getSQL")?.as_string().unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(true);
    }
    let plan = call1(
        db,
        "prepare",
        &JsValue::from_str(&format!("EXPLAIN {text}")),
    )?;
    let mut writes = false;
    let walked = (|| {
        while step(&plan).map_err(CallError::new)? {
            let row = Array::from(&call0(&plan, "get")?);
            let opcode = row.get(1).as_string().unwrap_or_default();
            let p2 = row.get(3).as_f64().unwrap_or(0.0);
            if (opcode == "Transaction" && p2 != 0.0) || WRITING_OPCODES.contains(&opcode.as_str())
            {
                writes = true;
            }
        }
        Ok::<(), CallError>(())
    })();
    free(&plan);
    walked.map(|()| !writes)
}

/// Whether no transaction is open: SQLite's own answer, from whether it
/// will start one. Native asks `sqlite3_get_autocommit`, which sql.js
/// does not export.
fn autocommit(db: &JsValue) -> bool {
    match run(db, "BEGIN") {
        Ok(()) => {
            let _ = run(db, "ROLLBACK");
            true
        }
        Err(_) => false,
    }
}

// depth: values

#[derive(Debug, Deserialize)]
struct SqlArgs {
    db: String,
    sql: String,
    #[serde(default)]
    params: Option<rmpv::Value>,
}

fn params_of(args: &SqlArgs) -> Result<Array, CallError> {
    let out = Array::new();
    let items = match &args.params {
        None | Some(rmpv::Value::Nil) => return Ok(out),
        Some(rmpv::Value::Map(m)) if m.is_empty() => return Ok(out),
        Some(rmpv::Value::Array(items)) => items,
        Some(other) => {
            return Err(CallError::new(format!(
                "params is a {other:?}; it is a list of values"
            )))
        }
    };
    for (i, v) in items.iter().enumerate() {
        out.push(&to_sql(v, i + 1)?);
    }
    Ok(out)
}

fn to_sql(value: &rmpv::Value, index: usize) -> Result<JsValue, CallError> {
    use rmpv::Value as V;
    Ok(match value {
        V::Nil => JsValue::NULL,
        V::Boolean(b) => JsValue::from_f64(f64::from(u8::from(*b))),
        V::Integer(n) => match n.as_i64() {
            Some(i) => JsValue::from_f64(i as f64),
            None => {
                return Err(CallError::new(format!(
                    "parameter {index} does not fit an i64"
                )))
            }
        },
        V::F32(f) => JsValue::from_f64(f64::from(*f)),
        V::F64(f) => JsValue::from_f64(*f),
        V::String(s) => match s.as_str() {
            Some(text) => JsValue::from_str(text),
            None => Uint8Array::from(s.as_bytes()).into(),
        },
        V::Binary(b) => Uint8Array::from(b.as_slice()).into(),
        other => {
            return Err(CallError::new(format!(
                "parameter {index} is a {other:?}, which SQLite has no type for"
            )))
        }
    })
}

/// One cell, as native's `from_sql`: text as bytes, an integer (a BigInt,
/// asked for with `useBigInt`) as an integer, a number as a real.
fn from_sql(value: &JsValue) -> rmpv::Value {
    if value.is_null() || value.is_undefined() {
        return rmpv::Value::Nil;
    }
    if value.is_bigint() {
        let text = js_sys::BigInt::unchecked_from_js_ref(value)
            .to_string(10)
            .map(String::from)
            .unwrap_or_default();
        return text
            .parse::<i64>()
            .map(rmpv::Value::from)
            .unwrap_or(rmpv::Value::Nil);
    }
    if let Some(f) = value.as_f64() {
        return rmpv::Value::from(f);
    }
    if let Some(s) = value.as_string() {
        return rmpv::Value::Binary(s.into_bytes());
    }
    if let Some(b) = value.dyn_ref::<Uint8Array>() {
        return rmpv::Value::Binary(b.to_vec());
    }
    rmpv::Value::Nil
}

// depth: the scope, kept to native's shape and words

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SqlScope {
    scope: PathBuf,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    max_result_rows: Option<usize>,
    #[serde(default)]
    create: Option<bool>,
    #[serde(default)]
    journal_mode: Option<String>,
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
                journal_mode: None,
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
        if let Some(mode) = &parsed.journal_mode {
            if !JOURNAL_MODES.contains(&mode.to_ascii_lowercase().as_str()) {
                return Err(format!(
                    "config.connectors.sql.journal_mode must be one of {} (got '{mode}')",
                    JOURNAL_MODES.join(", ")
                ));
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

    fn root(&self, fs: &dyn Backend) -> Result<PathBuf, String> {
        fs.canonicalize(&self.scope).map_err(|e| {
            format!(
                "scope directory {} cannot be resolved: {e}",
                self.scope.display()
            )
        })
    }

    fn resolve_db(&self, fs: &dyn Backend, name: &str) -> Result<PathBuf, String> {
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
        let root = self.root(fs)?;
        let path = root.join(name);
        if fs.metadata(&path).is_ok() {
            let resolved = fs
                .canonicalize(&path)
                .map_err(|e| format!("'{name}': {e}"))?;
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

struct SqlJsScopeType {
    fs: Arc<dyn Backend>,
}

impl ScopeType for SqlJsScopeType {
    fn describe(&self) -> &str {
        "a directory: \"path\", or {scope, access?: read|readwrite, max_result_rows?, create?, journal_mode?: wal|delete|truncate|persist|memory|off}"
    }

    fn validate(&self, scope: Option<&Scope>) -> Result<(), String> {
        SqlScope::parse(scope)?.root(self.fs.as_ref()).map(|_| ())
    }
}

// depth: calling into sql.js

fn method(object: &JsValue, name: &str) -> Result<Function, String> {
    Reflect::get(object, &JsValue::from_str(name))
        .ok()
        .and_then(|f| f.dyn_into::<Function>().ok())
        .ok_or_else(|| format!("the page's sql backing has no '{name}'"))
}

fn call0(object: &JsValue, name: &str) -> Result<JsValue, CallError> {
    method(object, name)
        .map_err(CallError::new)?
        .call0(object)
        .map_err(|e| CallError::new(message(&e)))
}

fn call1(object: &JsValue, name: &str, arg: &JsValue) -> Result<JsValue, CallError> {
    method(object, name)
        .map_err(CallError::new)?
        .call1(object, arg)
        .map_err(|e| CallError::new(format!("preparing: {}", message(&e))))
}

fn bind(stmt: &JsValue, params: &Array) -> Result<(), CallError> {
    if params.length() == 0 {
        return Ok(());
    }
    method(stmt, "bind")
        .map_err(CallError::new)?
        .call1(stmt, params)
        .map(|_| ())
        .map_err(|e| CallError::new(format!("binding: {}", message(&e))))
}

fn step(stmt: &JsValue) -> Result<bool, String> {
    method(stmt, "step")?
        .call0(stmt)
        .map(|v| v.as_bool() == Some(true))
        .map_err(|e| message(&e))
}

fn free(stmt: &JsValue) {
    if let Ok(f) = method(stmt, "free") {
        let _ = f.call0(stmt);
    }
}

fn run(db: &JsValue, sql: &str) -> Result<(), String> {
    method(db, "run")?
        .call1(db, &JsValue::from_str(sql))
        .map(|_| ())
        .map_err(|e| message(&e))
}

/// The first column of the first row of `sql`.
fn scalar(db: &JsValue, sql: &str) -> Result<JsValue, String> {
    let stmt = method(db, "prepare")?
        .call1(db, &JsValue::from_str(sql))
        .map_err(|e| message(&e))?;
    let out = (|| {
        if !step(&stmt)? {
            return Ok(JsValue::NULL);
        }
        let options = js_sys::Object::new();
        let _ = Reflect::set(&options, &"useBigInt".into(), &JsValue::TRUE);
        let row = method(&stmt, "get")?
            .call2(&stmt, &JsValue::NULL, &options)
            .map_err(|e| message(&e))?;
        Ok(Array::from(&row).get(0))
    })();
    free(&stmt);
    out
}

fn message(thrown: &JsValue) -> String {
    match thrown.dyn_ref::<js_sys::Error>() {
        Some(e) => String::from(e.message()),
        None => thrown.as_string().unwrap_or_else(|| format!("{thrown:?}")),
    }
}
