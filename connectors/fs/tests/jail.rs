//! The fs connector against a real directory. The escape tests are the
//! point: a jail that only checks the string it was handed is not a jail,
//! so `..`, absolute paths, and symlinks pointing out are all exercised
//! against the resolved path.

use std::sync::Arc;

use drt_caps::{CapSet, Grant, Scope};
use drt_connector::{Connector, Dispatcher, Registry};
use drt_connector_fs::FsConnector;
use drt_hostcall::{to_bytes, Request, Status};

fn scope(dir: &std::path::Path, access: &str, max_bytes: u64) -> Scope {
    Scope(rmpv::Value::Map(vec![
        ("scope".into(), dir.to_str().unwrap().into()),
        ("access".into(), access.into()),
        ("max_bytes".into(), rmpv::Value::from(max_bytes)),
    ]))
}

fn read_args(path: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![("path".into(), path.into())])
}

fn write_args(path: &str, data: &str, append: bool) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("path".into(), path.into()),
        ("data".into(), data.into()),
        ("append".into(), rmpv::Value::Boolean(append)),
    ])
}

fn call(sc: &Scope, name: &str, args: rmpv::Value) -> Result<rmpv::Value, String> {
    pollster::block_on(FsConnector::new().call(name, Some(args), Some(sc)))
        .map_err(|e| e.to_string())
}

/// The cap6 workload, verb for verb: write, read back, append, read again.
#[test]
fn the_cap6_shape_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let sc = scope(dir.path(), "readwrite", 65536);

    call(
        &sc,
        "fs/write",
        write_args("cap6_note.txt", "{\"a\":1}", false),
    )
    .unwrap();
    let got = call(&sc, "fs/read", read_args("cap6_note.txt")).unwrap();
    assert_eq!(got.as_slice().unwrap(), b"{\"a\":1}");

    call(
        &sc,
        "fs/write",
        write_args("cap6_log.txt", "first\n", false),
    )
    .unwrap();
    call(
        &sc,
        "fs/write",
        write_args("cap6_log.txt", "second\n", true),
    )
    .unwrap();
    let log = call(&sc, "fs/read", read_args("cap6_log.txt")).unwrap();
    assert_eq!(
        log.as_slice().unwrap(),
        b"first\nsecond\n",
        "append adds rather than replacing"
    );

    let listing = call(&sc, "fs/list", rmpv::Value::Nil).unwrap();
    let names: Vec<_> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(names, ["cap6_log.txt", "cap6_note.txt"]);

    call(&sc, "fs/remove", read_args("cap6_note.txt")).unwrap();
    assert!(call(&sc, "fs/read", read_args("cap6_note.txt")).is_err());
}

/// Bytes are bytes: a Lua string is not required to be UTF-8, and neither
/// direction may corrupt one.
#[test]
fn arbitrary_bytes_survive_the_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let sc = scope(dir.path(), "readwrite", 65536);
    let raw: Vec<u8> = vec![0x00, 0xff, 0x1b, 0x80, b'a', 0x00];
    let args = rmpv::Value::Map(vec![
        ("path".into(), "bin.dat".into()),
        ("data".into(), rmpv::Value::Binary(raw.clone())),
    ]);
    call(&sc, "fs/write", args).unwrap();
    let got = call(&sc, "fs/read", read_args("bin.dat")).unwrap();
    assert_eq!(got.as_slice().unwrap(), &raw[..]);
}

#[test]
fn escapes_are_refused_on_the_resolved_path() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"not yours").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let sc = scope(dir.path(), "readwrite", 65536);

    // A traversal, an absolute path, and a nested traversal.
    for bad in ["../secret.txt", "../../etc/passwd", "a/../../secret.txt"] {
        let err = call(&sc, "fs/read", read_args(bad)).unwrap_err();
        assert!(
            err.contains("outside the granted scope"),
            "{bad} must read as a jail refusal whether or not the target exists: {err}"
        );
    }
    let err = call(&sc, "fs/read", read_args("/etc/passwd")).unwrap_err();
    assert!(err.contains("absolute"), "{err}");

    // A symlink whose *target* is outside: caught because the check is on
    // the resolved path, not the string.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link"))
            .unwrap();
        let err = call(&sc, "fs/read", read_args("link")).unwrap_err();
        assert!(
            err.contains("outside the granted scope"),
            "symlink escaped: {err}"
        );

        // And a symlinked *parent* on the write path, where the file itself
        // does not exist yet.
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dir")).unwrap();
        let err = call(&sc, "fs/write", write_args("dir/new.txt", "x", false)).unwrap_err();
        assert!(
            err.contains("outside the granted scope"),
            "symlinked parent escaped: {err}"
        );
    }
}

#[test]
fn read_is_the_default_and_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("there.txt"), b"readable").unwrap();

    // The bare-string scope form takes the default, which is read.
    let sc = Scope(rmpv::Value::from(dir.path().to_str().unwrap()));
    assert_eq!(
        call(&sc, "fs/read", read_args("there.txt"))
            .unwrap()
            .as_slice()
            .unwrap(),
        b"readable"
    );
    for verb in ["fs/write", "fs/remove"] {
        let args = if verb == "fs/write" {
            write_args("there.txt", "nope", false)
        } else {
            read_args("there.txt")
        };
        let err = call(&sc, verb, args).unwrap_err();
        assert!(err.contains("readwrite"), "{verb} was not refused: {err}");
    }
}

#[test]
fn the_size_cap_bounds_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let sc = scope(dir.path(), "readwrite", 16);
    // Writing past the cap.
    let err = call(
        &sc,
        "fs/write",
        write_args("big.txt", &"x".repeat(32), false),
    )
    .unwrap_err();
    assert!(err.contains("past the 16-byte cap"), "{err}");
    // Appending past it, counting what is already there.
    call(&sc, "fs/write", write_args("grow.txt", "0123456789", false)).unwrap();
    let err = call(&sc, "fs/write", write_args("grow.txt", "0123456789", true)).unwrap_err();
    assert!(err.contains("past the 16-byte cap"), "{err}");
    // Reading a file that grew past it behind our back.
    std::fs::write(dir.path().join("planted.txt"), vec![b'x'; 64]).unwrap();
    let err = call(&sc, "fs/read", read_args("planted.txt")).unwrap_err();
    assert!(err.contains("past the 16-byte cap"), "{err}");
}

#[test]
fn ill_scoped_wiring_fails_at_startup_by_name() {
    let mut registry = Registry::new();
    // No scope at all.
    let err = registry
        .wire("fs", Arc::new(FsConnector::new()), None)
        .unwrap_err();
    assert_eq!(err.capability, "host:fs");
    assert!(err.to_string().contains("scope is required"), "{err}");

    // A directory that is not there.
    let missing = Scope(rmpv::Value::from("/nonexistent/workspace"));
    let err = registry
        .wire("fs", Arc::new(FsConnector::new()), Some(missing))
        .unwrap_err();
    assert!(err.to_string().contains("cannot be resolved"), "{err}");

    // An access mode that is not one of the two.
    let dir = tempfile::tempdir().unwrap();
    let bad = Scope(rmpv::Value::Map(vec![
        ("scope".into(), dir.path().to_str().unwrap().into()),
        ("access".into(), "sometimes".into()),
    ]));
    let err = registry
        .wire("fs", Arc::new(FsConnector::new()), Some(bad))
        .unwrap_err();
    assert!(err.to_string().contains("\"read\""), "{err}");
}

/// Through the dispatcher: the grant gates the call before the connector
/// ever sees a path.
#[test]
fn the_grant_gates_it_before_the_filesystem_does() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("there.txt"), b"readable").unwrap();
    let mut registry = Registry::new();
    registry
        .wire(
            "fs",
            Arc::new(FsConnector::new()),
            Some(scope(dir.path(), "read", 65536)),
        )
        .unwrap();
    let dispatcher = Dispatcher::new(registry);
    let raw = to_bytes(&Request {
        tok: 3,
        call: "fs/read".into(),
        args: Some(read_args("there.txt")),
    })
    .unwrap();

    let granted = CapSet::root(vec![Grant::grant("host:fs/*")]);
    let reply = pollster::block_on(dispatcher.dispatch(&granted, &raw));
    assert_eq!(reply.status, Status::Ok);
    assert_eq!(reply.value.unwrap().as_slice().unwrap(), b"readable");

    // Narrowed to reads of a different family: denied, and the filesystem
    // is never touched.
    let narrow = CapSet::root(vec![Grant::grant("host:time")]);
    let reply = pollster::block_on(dispatcher.dispatch(&narrow, &raw));
    assert_eq!(reply.status, Status::Denied);
}

/// The C host settles the spelling. `dhost.c` accepts exactly `"read"` or
/// `"readwrite"` for `config.connectors.fs.access` and refuses everything
/// else by name, so DRT accepts exactly those two and refuses the rest —
/// `"readonly"` included, which DRT itself took through v0.3.0 and which
/// the C host has never accepted. A config that loads on one host loads on
/// the other, and this test is what keeps that true.
#[test]
fn the_access_spelling_is_the_c_hosts() {
    let dir = tempfile::tempdir().unwrap();
    for good in ["read", "readwrite"] {
        let mut registry = Registry::new();
        assert!(
            registry
                .wire(
                    "fs",
                    Arc::new(FsConnector::new()),
                    Some(scope(dir.path(), good, 4096)),
                )
                .is_ok(),
            "the C host accepts {good:?} and so must DRT"
        );
    }
    let mut registry = Registry::new();
    let err = registry
        .wire(
            "fs",
            Arc::new(FsConnector::new()),
            Some(scope(dir.path(), "readonly", 4096)),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("\"read\""), "{err}");
    assert!(err.contains("\"readwrite\""), "{err}");
}

/// The same jail over a page's memory filesystem (`drt_platform::fs::MemFs`).
///
/// The backend is the only thing that changes between a disk and a page,
/// so the refusals -- `..`, an absolute path, a read-only scope, the byte
/// cap -- must read identically there, and the verbs must round-trip. The
/// text is asserted rather than the kind because `doc/Wasm.md` §5 makes
/// `expected.txt` the oracle on every target: a message that differs in a
/// browser is a divergence a program can see.
#[test]
fn the_jail_holds_over_a_memory_backend() {
    use drt_platform::fs::MemFs;

    let mem = Arc::new(MemFs::new());
    mem.add_file("/work/note.txt", "from the granted directory");
    mem.add_file("/etc/passwd", "root:x:0:0");
    let connector = FsConnector::with_backend(mem.clone());
    let sc = scope(std::path::Path::new("/work"), "readwrite", 64);
    let call = |name: &str, args: rmpv::Value| {
        pollster::block_on(connector.call(name, Some(args), Some(&sc))).map_err(|e| e.to_string())
    };

    let got = call("fs/read", read_args("note.txt")).unwrap();
    assert_eq!(got.as_slice().unwrap(), b"from the granted directory");

    call("fs/write", write_args("log.txt", "first\n", false)).unwrap();
    call("fs/write", write_args("log.txt", "second\n", true)).unwrap();
    assert_eq!(
        call("fs/read", read_args("log.txt"))
            .unwrap()
            .as_slice()
            .unwrap(),
        b"first\nsecond\n"
    );
    let names: Vec<String> = call("fs/list", rmpv::Value::Nil)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["log.txt", "note.txt"]);
    call("fs/remove", read_args("log.txt")).unwrap();
    assert_eq!(
        mem.files().len(),
        2,
        "the page sees what the program wrote and removed"
    );

    // The refusals, worded as the disk words them.
    let escape = call("fs/read", read_args("../etc/passwd")).unwrap_err();
    assert!(
        escape.contains("resolves outside the granted scope"),
        "{escape}"
    );
    let absolute = call("fs/read", read_args("/etc/passwd")).unwrap_err();
    assert!(absolute.contains("is absolute"), "{absolute}");
    let big = call("fs/write", write_args("big.txt", &"x".repeat(65), false)).unwrap_err();
    assert!(big.contains("past the 64-byte cap"), "{big}");
    let missing = call("fs/read", read_args("missing.txt")).unwrap_err();
    assert!(missing.contains("missing.txt"), "{missing}");

    // And a scope naming a directory the page never seeded is a startup
    // refusal, by name, exactly as on disk.
    let mut reg = Registry::new();
    let err = reg
        .wire(
            "fs",
            Arc::new(FsConnector::with_backend(mem)),
            Some(scope(std::path::Path::new("/nowhere"), "read", 64)),
        )
        .unwrap_err();
    assert!(err.to_string().contains("cannot be resolved"), "{err}");
}
