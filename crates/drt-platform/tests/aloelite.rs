//! `AloeliteFs` against a real volume file: the Backend contract MemFs and
//! StdFs keep, what persists across a close, and the admission refusal.
#![cfg(feature = "aloelite")]

use std::io::ErrorKind;
use std::path::Path;

use drt_platform::aloelite::AloeliteFs;
use drt_platform::fs::Backend;

fn p(s: &str) -> &Path {
    Path::new(s)
}

#[test]
fn the_backend_contract_over_a_volume() {
    let dir = tempfile::tempdir().unwrap();
    let fs = AloeliteFs::open(&dir.path().join("v.fs"), "docs", "/", true).unwrap();

    fs.create_dir_all(p("/a/b")).unwrap();
    fs.create_dir_all(p("/a/b")).unwrap(); // already there is success
    fs.write(p("/a/b/note.txt"), b"one", false).unwrap();
    fs.write(p("/a/b/note.txt"), b" two", true).unwrap();
    assert_eq!(fs.read(p("/a/b/note.txt")).unwrap(), b"one two");
    assert_eq!(fs.read(p("/a/./b/../b/note.txt")).unwrap(), b"one two");
    let m = fs.metadata(p("/a/b/note.txt")).unwrap();
    assert_eq!((m.len, m.is_dir), (7, false));
    assert!(fs.metadata(p("/a")).unwrap().is_dir);
    assert_eq!(fs.canonicalize(p("/a/b/../b")).unwrap(), Path::new("/a/b"));
    assert_eq!(
        fs.canonicalize(p("/nope")).unwrap_err().kind(),
        ErrorKind::NotFound
    );

    fs.write(p("/a/z.txt"), b"z", false).unwrap();
    assert_eq!(fs.read_dir(p("/a")).unwrap(), vec!["b", "z.txt"]);

    // A missing parent, a directory written to or removed as a file.
    assert_eq!(
        fs.write(p("/x/y.txt"), b"", false).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        fs.write(p("/a"), b"", false).unwrap_err().kind(),
        ErrorKind::IsADirectory
    );
    assert_eq!(
        fs.remove_file(p("/a")).unwrap_err().kind(),
        ErrorKind::IsADirectory
    );
    assert_eq!(
        fs.read(p("/a")).unwrap_err().kind(),
        ErrorKind::IsADirectory
    );
    assert_eq!(
        fs.create_dir_all(p("/a/z.txt/q")).unwrap_err().kind(),
        ErrorKind::NotADirectory
    );

    fs.remove_file(p("/a/z.txt")).unwrap();
    assert_eq!(fs.read_dir(p("/a")).unwrap(), vec!["b"]);
    fs.remove_dir_all(p("/a")).unwrap();
    fs.remove_dir_all(p("/a")).unwrap(); // missing is success
    assert!(fs.read_dir(p("/")).unwrap().is_empty());
}

#[test]
fn files_outlive_the_handle_and_the_mount_point_is_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.fs");
    {
        let fs = AloeliteFs::open(&file, "docs", "/", true).unwrap();
        fs.create_dir_all(p("/home/k")).unwrap();
        fs.write(p("/home/k/kept.txt"), b"kept", false).unwrap();
    }
    // Dropped, so unmounted: a read-write mount is admitted again.
    let again = AloeliteFs::open(&file, "docs", "/", true).unwrap();
    assert_eq!(again.read(p("/home/k/kept.txt")).unwrap(), b"kept");
    drop(again);
    // Mounted lower down, that directory is this backend's `/`.
    let lower = AloeliteFs::open(&file, "docs", "/home/k", false).unwrap();
    assert_eq!(lower.read(p("/kept.txt")).unwrap(), b"kept");
    assert_eq!(lower.read_dir(p("/")).unwrap(), vec!["kept.txt"]);
    // Read-only: aloelite refuses the write.
    assert!(lower.write(p("/kept.txt"), b"changed", false).is_err());
    assert_eq!(lower.read(p("/kept.txt")).unwrap(), b"kept");
}

#[test]
fn a_second_volume_by_name_is_a_second_tree() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.fs");
    let a = AloeliteFs::open(&file, "a", "/", true).unwrap();
    a.write(p("/only-a.txt"), b"a", false).unwrap();
    let b = AloeliteFs::open(&file, "b", "/", true).unwrap();
    assert!(b.read_dir(p("/")).unwrap().is_empty());
    assert_eq!(a.read_dir(p("/")).unwrap(), vec!["only-a.txt"]);
}

#[test]
fn a_live_read_write_mount_refuses_another_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.fs");
    let _held = AloeliteFs::open(&file, "docs", "/", true).unwrap();
    let refused = AloeliteFs::open(&file, "docs", "/", true).err().unwrap();
    assert!(refused.contains("volume 'docs' at '/'"), "{refused}");
    assert!(refused.contains("rw mount"), "{refused}");
    // A read-only one is admitted beside it.
    AloeliteFs::open(&file, "docs", "/", false).unwrap();
}

#[test]
fn a_mount_point_the_volume_lacks_is_refused_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let refused = AloeliteFs::open(&dir.path().join("v.fs"), "docs", "/no/such", true)
        .err()
        .unwrap();
    assert!(refused.contains("'/no/such'"), "{refused}");
}

#[test]
fn a_file_closed_cleanly_is_left_out_of_wal() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("v.fs");
    let fs = AloeliteFs::open(&file, "docs", "/", true).unwrap();
    fs.write(p("/a.txt"), b"a", false).unwrap();
    drop(fs);
    let conn = rusqlite::Connection::open(&file).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}
