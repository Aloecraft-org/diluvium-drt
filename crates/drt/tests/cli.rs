//! `drt run` end to end, through the real binary: a config file names the
//! ceiling and wires a connector to a place, a program names a file inside
//! that place, and the answer comes back over the hostcall pump.

use std::path::Path;
use std::process::Command;

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// A guest that makes one hostcall and prints the reply's status, so the
/// test reads the capability decision rather than inferring it.
const CALLER: &str = r#"
local calls = queue.declare("host/calls", { capacity = 4, exported = true, on_full = "reject" })
local replies = queue.declare("host/replies", { capacity = 4 })
queue.push(calls, { tok = 1, call = "fs/read", args = { path = "note.txt" } })
local _, reply = queue.wait({replies})
print(reply.status .. "|" .. tostring(reply.value or reply.detail))
"#;

#[test]
fn a_config_wires_a_connector_to_a_place_and_the_program_names_a_file_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    write(&work.join("note.txt"), "from the granted directory");
    write(&dir.path().join("prog.dlua"), CALLER);
    write(
        &dir.path().join("drt.json"),
        &format!(
            r#"{{
              "caps": [{{"capability": "host:fs/*"}}],
              "connectors": {{
                "fs": {{"scope": {{"scope": "{}", "access": "read"}}}}
              }}
            }}"#,
            work.to_str().unwrap()
        ),
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("ok|from the granted directory"),
        "the program read its file through the granted scope: {stdout}"
    );
}

#[test]
fn a_ceiling_that_does_not_cover_the_call_denies_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    write(&work.join("note.txt"), "unreachable");
    write(&dir.path().join("prog.dlua"), CALLER);
    // fs is wired, but the ceiling grants only time: the connector is there
    // and the call still does not happen.
    write(
        &dir.path().join("drt.json"),
        &format!(
            r#"{{
              "caps": [{{"capability": "host:time"}}],
              "connectors": {{
                "fs": {{"scope": {{"scope": "{}"}}}}
              }}
            }}"#,
            work.to_str().unwrap()
        ),
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("denied|"),
        "outside the ceiling is denied, not read: {stdout}"
    );
}

#[test]
fn an_ill_scoped_config_fails_at_startup_by_name() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("prog.dlua"), "return 1");
    write(
        &dir.path().join("drt.json"),
        r#"{"connectors": {"fs": {"scope": {"scope": "/nonexistent/place"}}}}"#,
    );

    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("cannot be resolved"),
        "the refusal names the fix: {stderr}"
    );
}

#[test]
fn a_config_may_name_the_program_and_the_argument_wins() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("from_config.dlua"), "print('config')");
    write(&dir.path().join("from_argv.dlua"), "print('argv')");
    let cfg = dir.path().join("drt.json");
    write(
        &cfg,
        &format!(
            r#"{{"program": {{"path": "{}"}}}}"#,
            dir.path().join("from_config.dlua").to_str().unwrap()
        ),
    );

    // No argument: the config's program runs.
    let out = drt().arg("run").arg("--config").arg(&cfg).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("config"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An argument overrides it: it is the more specific thing just typed.
    let out = drt()
        .arg("run")
        .arg(dir.path().join("from_argv.dlua"))
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("argv"));
}

#[test]
fn no_program_anywhere_says_so() {
    let out = drt().arg("run").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("name a program"));
}
