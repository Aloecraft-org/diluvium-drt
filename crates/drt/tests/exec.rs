//! `exec/run` through the real binary: a config wires exec and is told so,
//! the C host's `exec = true` loads unchanged, and a ceiling that does not
//! cover the call denies it before any process starts.

#![cfg(feature = "connector-exec")]

use std::path::Path;
use std::process::Command;

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// A guest that runs one command through the `host` library and prints the
/// reply the way the examples do: status, then value or detail.
const CALLER: &str = r#"
local value, status, detail = host.exec.try_run({ "sh", "-c", "echo hi; exit 3" })
if value then
    print(status .. "|" .. value.status .. "|" .. value.stdout)
else
    print(status .. "|" .. detail)
end
"#;

#[test]
fn a_config_that_wires_exec_is_announced_and_answers() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("prog.dlua"), CALLER);
    write(
        &dir.path().join("drt.json"),
        r#"{
          "caps": [{"capability": "host:exec/run"}],
          "connectors": {
            "exec": {"scope": {"max_timeout_ms": 5000, "allow": ["/bin/sh"]}}
          }
        }"#,
    );
    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stdout.contains("ok|3|hi\n"),
        "the exit is an answer and the output is the child's: {stdout}"
    );
    assert!(
        stderr.contains("drt: exec wired: granting host:exec/run leaves the sandbox"),
        "wiring exec is announced, not implied: {stderr}"
    );
}

#[test]
fn the_c_hosts_exec_true_loads_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("prog.dlua"), CALLER);
    write(
        &dir.path().join("app.host.lua"),
        r#"
        return {
          supervisor = "prog.dlua",
          caps = { "host:exec/run" },
          connectors = { exec = true },
        }
        "#,
    );
    let out = drt()
        .arg("run")
        .arg("--config")
        .arg(dir.path().join("app.host.lua"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("ok|3|hi\n"), "{stdout}");
}

#[test]
fn a_ceiling_that_does_not_cover_the_call_denies_it_before_anything_runs() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("prog.dlua"), CALLER);
    write(
        &dir.path().join("drt.json"),
        r#"{
          "caps": [{"capability": "host:time"}],
          "connectors": { "exec": {} }
        }"#,
    );
    let out = drt()
        .arg("run")
        .arg(dir.path().join("prog.dlua"))
        .arg("--config")
        .arg(dir.path().join("drt.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(
        stdout.contains("denied|'exec/run' is outside this instance's grants"),
        "{stdout}"
    );
}
