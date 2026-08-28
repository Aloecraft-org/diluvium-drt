//! The sql connector against real SQLite. The cap2 workload verb for verb,
//! plus the refusals that make the scope a scope.

use std::sync::Arc;

use drt_caps::{CapSet, Grant, Scope};
use drt_connector::{Connector, Dispatcher, Registry};
use drt_connector_sql::SqlConnector;
use drt_hostcall::{to_bytes, Request, Status};

fn scope(dir: &std::path::Path, access: &str, max_rows: u64) -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("scope".into(), dir.to_str().unwrap().into()),
        ("access".into(), access.into()),
        ("max_result_rows".into(), rmpv::Value::from(max_rows)),
    ]))
}

fn args(db: &str, sql: &str, params: Vec<rmpv::Value>) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("db".into(), db.into()),
        ("sql".into(), sql.into()),
        ("params".into(), rmpv::Value::Array(params)),
    ])
}

fn call(c: &SqlConnector, sc: &Scope, name: &str, a: rmpv::Value) -> Result<rmpv::Value, String> {
    pollster::block_on(c.call(name, Some(a), Some(sc))).map_err(|e| e.to_string())
}

fn field<'a>(v: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    v.as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or(&rmpv::Value::Nil)
}

/// cap2's shape: create a table, write a value, read it back.
#[test]
fn the_cap2_workload_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let c = SqlConnector::new();
    let sc = scope(dir.path(), "readwrite", 64);

    call(
        &c,
        &sc,
        "sql/exec",
        args(
            "data.sqlite",
            "CREATE TABLE IF NOT EXISTS cap2_kv (k TEXT PRIMARY KEY, v TEXT)",
            vec![],
        ),
    )
    .unwrap();

    let wrote = call(
        &c,
        &sc,
        "sql/exec",
        args(
            "data.sqlite",
            "INSERT OR REPLACE INTO cap2_kv (k, v) VALUES (?, ?)",
            vec!["demo".into(), r#"{"n":1}"#.into()],
        ),
    )
    .unwrap();
    assert_eq!(field(&wrote, "changes").as_u64(), Some(1));

    let got = call(
        &c,
        &sc,
        "sql/query",
        args(
            "data.sqlite",
            "SELECT v FROM cap2_kv WHERE k = ?",
            vec!["demo".into()],
        ),
    )
    .unwrap();
    let cols: Vec<_> = field(&got, "cols")
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(cols, ["v"]);
    let rows = field(&got, "rows").as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].as_array().unwrap()[0].as_slice().unwrap(),
        br#"{"n":1}"#
    );
}

/// Multiple databases inside one granted scope fall out for free — the
/// program names them, the config named only the place.
#[test]
fn a_program_may_name_more_than_one_database_in_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    let c = SqlConnector::new();
    let sc = scope(dir.path(), "readwrite", 64);
    for db in ["one.sqlite", "two.sqlite"] {
        call(
            &c,
            &sc,
            "sql/exec",
            args(db, "CREATE TABLE t (x INT)", vec![]),
        )
        .unwrap();
        call(
            &c,
            &sc,
            "sql/exec",
            args(db, "INSERT INTO t VALUES (?)", vec![rmpv::Value::from(7)]),
        )
        .unwrap();
    }
    let got = call(
        &c,
        &sc,
        "sql/query",
        args("one.sqlite", "SELECT x FROM t", vec![]),
    )
    .unwrap();
    assert_eq!(
        field(&got, "rows").as_array().unwrap()[0]
            .as_array()
            .unwrap()[0]
            .as_i64(),
        Some(7)
    );
}

/// `query` refuses a statement that writes, by SQLite's own classification.
#[test]
fn query_refuses_a_statement_that_writes() {
    let dir = tempfile::tempdir().unwrap();
    let c = SqlConnector::new();
    let sc = scope(dir.path(), "readwrite", 64);
    call(
        &c,
        &sc,
        "sql/exec",
        args("d.sqlite", "CREATE TABLE t (x INT)", vec![]),
    )
    .unwrap();

    let err = call(
        &c,
        &sc,
        "sql/query",
        args("d.sqlite", "INSERT INTO t VALUES (1)", vec![]),
    )
    .unwrap_err();
    assert!(err.contains("for statements that read"), "{err}");
    assert!(err.contains("different grant"), "{err}");

    // And a read through query is fine.
    call(
        &c,
        &sc,
        "sql/query",
        args("d.sqlite", "SELECT * FROM t", vec![]),
    )
    .unwrap();
}

/// `exec` exists only where the config granted writes. A read scope is
/// exactly what it says.
#[test]
fn exec_needs_the_write_grant() {
    let dir = tempfile::tempdir().unwrap();
    // Seed a database with a writable scope, then reopen the place read-only.
    {
        let c = SqlConnector::new();
        let rw = scope(dir.path(), "readwrite", 64);
        call(
            &c,
            &rw,
            "sql/exec",
            args("d.sqlite", "CREATE TABLE t (x INT)", vec![]),
        )
        .unwrap();
    }
    let c = SqlConnector::new();
    let ro = scope(dir.path(), "read", 64);
    let err = call(
        &c,
        &ro,
        "sql/exec",
        args("d.sqlite", "INSERT INTO t VALUES (1)", vec![]),
    )
    .unwrap_err();
    assert!(err.contains("readwrite"), "{err}");
    // Reading still works.
    call(
        &c,
        &ro,
        "sql/query",
        args("d.sqlite", "SELECT * FROM t", vec![]),
    )
    .unwrap();
}

/// A read scope does not conjure databases either.
#[test]
fn a_read_scope_does_not_create() {
    let dir = tempfile::tempdir().unwrap();
    let c = SqlConnector::new();
    let ro = scope(dir.path(), "read", 64);
    let err = call(
        &c,
        &ro,
        "sql/query",
        args("absent.sqlite", "SELECT 1", vec![]),
    )
    .unwrap_err();
    assert!(err.contains("does not create databases"), "{err}");
}

/// The row cap refuses rather than truncating: a truncated result is a
/// silent lie.
#[test]
fn the_row_cap_refuses_rather_than_truncating() {
    let dir = tempfile::tempdir().unwrap();
    let c = SqlConnector::new();
    let sc = scope(dir.path(), "readwrite", 4);
    call(
        &c,
        &sc,
        "sql/exec",
        args("d.sqlite", "CREATE TABLE t (x INT)", vec![]),
    )
    .unwrap();
    for i in 0..10 {
        call(
            &c,
            &sc,
            "sql/exec",
            args(
                "d.sqlite",
                "INSERT INTO t VALUES (?)",
                vec![rmpv::Value::from(i)],
            ),
        )
        .unwrap();
    }
    let err = call(
        &c,
        &sc,
        "sql/query",
        args("d.sqlite", "SELECT x FROM t", vec![]),
    )
    .unwrap_err();
    assert!(err.contains("past the 4-row cap"), "{err}");
    assert!(
        err.contains("LIMIT/OFFSET"),
        "the refusal says how to page: {err}"
    );
    // Paging within the cap works.
    let got = call(
        &c,
        &sc,
        "sql/query",
        args("d.sqlite", "SELECT x FROM t LIMIT 3", vec![]),
    )
    .unwrap();
    assert_eq!(field(&got, "rows").as_array().unwrap().len(), 3);
}

/// A database is a *name* inside the scope, never a path.
#[test]
fn a_database_is_a_name_not_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let c = SqlConnector::new();
    let sc = scope(dir.path(), "readwrite", 64);
    for bad in ["../escape.sqlite", "sub/d.sqlite", "..", ".", "a\\b.sqlite"] {
        let err = call(&c, &sc, "sql/query", args(bad, "SELECT 1", vec![])).unwrap_err();
        assert!(
            err.contains("path separator") || err.contains("not a database name"),
            "'{bad}' was not refused as a name: {err}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_scope_is_refused_not_clamped() {
    let outside = tempfile::tempdir().unwrap();
    // A real database outside the scope.
    {
        let c = SqlConnector::new();
        let sc = scope(outside.path(), "readwrite", 64);
        call(
            &c,
            &sc,
            "sql/exec",
            args("secret.sqlite", "CREATE TABLE t (x INT)", vec![]),
        )
        .unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.sqlite"),
        dir.path().join("link.sqlite"),
    )
    .unwrap();

    let c = SqlConnector::new();
    let sc = scope(dir.path(), "readwrite", 64);
    let err = call(
        &c,
        &sc,
        "sql/query",
        args("link.sqlite", "SELECT 1", vec![]),
    )
    .unwrap_err();
    assert!(err.contains("outside the granted scope"), "{err}");
}

#[test]
fn ill_scoped_wiring_fails_at_startup_by_name() {
    let mut registry = Registry::new();
    let err = registry
        .wire("sql", Arc::new(SqlConnector::new()), None)
        .unwrap_err();
    assert_eq!(err.capability, "host:sql");
    assert!(err.to_string().contains("scope is required"), "{err}");

    let missing = Scope(rmpv::Value::from("/nonexistent/workspace"));
    let err = registry
        .wire("sql", Arc::new(SqlConnector::new()), Some(missing))
        .unwrap_err();
    assert!(err.to_string().contains("cannot be resolved"), "{err}");
}

/// Through the dispatcher: the grant splits with the verbs, which is the
/// point of there being two of them.
#[test]
fn the_grant_splits_with_the_verbs() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = Registry::new();
    registry
        .wire(
            "sql",
            Arc::new(SqlConnector::new()),
            Some(scope(dir.path(), "readwrite", 64)),
        )
        .unwrap();
    let dispatcher = Dispatcher::new(registry);

    let exec = to_bytes(&Request {
        tok: 1,
        call: "sql/exec".into(),
        args: Some(args("d.sqlite", "CREATE TABLE t (x INT)", vec![])),
    })
    .unwrap();
    let query = to_bytes(&Request {
        tok: 2,
        call: "sql/query".into(),
        args: Some(args("d.sqlite", "SELECT 1", vec![])),
    })
    .unwrap();

    // A read-only grant reaches query and not exec, even though the scope
    // itself is readwrite: the capability is the narrower of the two.
    let reader = CapSet::root(vec![Grant::grant("host:sql/query")]);
    assert_eq!(
        pollster::block_on(dispatcher.dispatch(&reader, &exec)).status,
        Status::Denied
    );
    assert_eq!(
        pollster::block_on(dispatcher.dispatch(&reader, &query)).status,
        Status::Ok
    );

    // The family wildcard reaches both — the bigger thing it says.
    let both = CapSet::root(vec![Grant::grant("host:sql/*")]);
    assert_eq!(
        pollster::block_on(dispatcher.dispatch(&both, &exec)).status,
        Status::Ok
    );
}
