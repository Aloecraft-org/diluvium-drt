//! The fs connector over an aloelite volume (`volume_file` scopes): the
//! same four verbs and the same jail as a directory, rooted at the
//! volume's mount point, with files that outlive the connector.
#![cfg(feature = "aloelite")]

use drt_caps::{Scope, ScopeType};
use drt_connector::Connector;
use drt_connector_fs::FsConnector;

fn volume(file: &std::path::Path, access: &str, extra: &[(&str, rmpv::Value)]) -> Scope {
    let mut map = vec![
        ("volume_file".into(), file.to_str().unwrap().into()),
        ("access".into(), access.into()),
    ];
    for (k, v) in extra {
        map.push(((*k).into(), v.clone()));
    }
    Scope(rmpv::Value::Map(map))
}

fn path(p: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![("path".into(), p.into())])
}

fn write(p: &str, data: &str) -> rmpv::Value {
    rmpv::Value::Map(vec![
        ("path".into(), p.into()),
        ("data".into(), data.into()),
    ])
}

fn call(c: &FsConnector, sc: &Scope, name: &str, args: rmpv::Value) -> Result<rmpv::Value, String> {
    pollster::block_on(c.call(name, Some(args), Some(sc))).map_err(|e| e.to_string())
}

#[test]
fn the_four_verbs_and_the_jail_over_a_volume() {
    let dir = tempfile::tempdir().unwrap();
    let sc = volume(
        &dir.path().join("app.fs"),
        "readwrite",
        &[("max_bytes", 16.into())],
    );
    let c = FsConnector::new();
    c.scope_type().validate(Some(&sc)).unwrap();

    call(&c, &sc, "fs/write", write("note.txt", "in a volume")).unwrap();
    let got = call(&c, &sc, "fs/read", path("note.txt")).unwrap();
    assert_eq!(got.as_slice().unwrap(), b"in a volume");
    let names = call(&c, &sc, "fs/list", path(".")).unwrap();
    assert_eq!(names.as_array().unwrap().len(), 1);

    let e = call(&c, &sc, "fs/read", path("../note.txt")).unwrap_err();
    assert!(e.contains("resolves outside the granted scope"), "{e}");
    let e = call(&c, &sc, "fs/read", path("/etc/hosts")).unwrap_err();
    assert!(e.contains("is absolute"), "{e}");
    let e = call(&c, &sc, "fs/write", write("big.txt", "seventeen bytes!!")).unwrap_err();
    assert!(e.contains("past the 16-byte cap"), "{e}");

    call(&c, &sc, "fs/remove", path("note.txt")).unwrap();
    assert!(call(&c, &sc, "fs/list", path("."))
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn files_outlive_the_connector_and_read_scopes_are_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("app.fs");
    {
        let c = FsConnector::new();
        let sc = volume(&file, "readwrite", &[("volume", "docs".into())]);
        call(&c, &sc, "fs/write", write("kept.txt", "kept")).unwrap();
    }
    let c = FsConnector::new();
    let sc = volume(&file, "read", &[("volume", "docs".into())]);
    let got = call(&c, &sc, "fs/read", path("kept.txt")).unwrap();
    assert_eq!(got.as_slice().unwrap(), b"kept");
    let e = call(&c, &sc, "fs/write", write("kept.txt", "x")).unwrap_err();
    assert!(e.contains("read-only"), "{e}");
    // Another volume in the same file is another tree.
    let other = volume(&file, "read", &[("volume", "other".into())]);
    assert!(call(&c, &other, "fs/list", path("."))
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn a_mount_point_roots_the_jail_inside_the_volume() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("app.fs");
    {
        let c = FsConnector::new();
        let root = volume(&file, "readwrite", &[]);
        call(&c, &root, "fs/write", write("top.txt", "top")).unwrap();
    }
    // fs/write makes no directories, so the inner one is made through the
    // backend.
    {
        use drt_platform::fs::Backend;
        let fs = drt_platform::aloelite::AloeliteFs::open(&file, "drt", "/", true).unwrap();
        fs.create_dir_all(std::path::Path::new("/inner")).unwrap();
        fs.write(std::path::Path::new("/inner/in.txt"), b"in", false)
            .unwrap();
    }
    let c = FsConnector::new();
    let sc = volume(&file, "read", &[("mount_point", "/inner".into())]);
    let got = call(&c, &sc, "fs/read", path("in.txt")).unwrap();
    assert_eq!(got.as_slice().unwrap(), b"in");
    let e = call(&c, &sc, "fs/read", path("../top.txt")).unwrap_err();
    assert!(e.contains("resolves outside the granted scope"), "{e}");
}

#[test]
fn what_cannot_open_is_refused_at_startup_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let c = FsConnector::new();
    let missing_point = volume(
        &dir.path().join("a.fs"),
        "read",
        &[("mount_point", "/no".into())],
    );
    let e = c.scope_type().validate(Some(&missing_point)).unwrap_err();
    assert!(e.contains("'/no'"), "{e}");
    let bad_access = volume(&dir.path().join("a.fs"), "write", &[]);
    let e = c.scope_type().validate(Some(&bad_access)).unwrap_err();
    assert!(e.contains("\"read\" or \"readwrite\""), "{e}");
    let unknown = volume(&dir.path().join("a.fs"), "read", &[("pin_env", "X".into())]);
    let e = c.scope_type().validate(Some(&unknown)).unwrap_err();
    assert!(e.contains("pin_env"), "{e}");
    let not_a_file = volume(dir.path(), "read", &[]);
    let e = c.scope_type().validate(Some(&not_a_file)).unwrap_err();
    assert!(e.contains("volume file"), "{e}");
}
