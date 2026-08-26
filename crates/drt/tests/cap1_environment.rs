//! cap1, ported from discofetch `capability_testing/cap1_environment`.
//!
//! **What cap1 proves upstream:** the runtime is fully assembled and starts
//! from nothing — and specifically that there are *two* binaries from *two*
//! sources, because "which binary you have" is the first thing to get
//! wrong. `diluvium` (the CLI, from the website installer) is an unsealed
//! interpreter with no connectors, no listener and no driven swarm; those
//! live in `diluvium-host`, which the installer does not ship at all.
//!
//! **What it proves here:** the same assembled-runtime property, against a
//! runtime where that split has been designed out. SPEC.md §1 makes
//! diluvium purely the language and DRT the runtime environment that embeds
//! it, so the cliff cap1 exists to isolate has no edge to fall off: there is
//! one binary, it carries the engine, and installing it gets you
//! everything. The port therefore asserts the property cap1 was reaching
//! for rather than transcribing its two-binary check, and pins the claim so
//! it cannot quietly stop being true.
//!
//! The blank slate carries over directly: a runtime that starts from
//! nothing grants nothing, and this asserts the empty config really is
//! locked rather than conveniently permissive.

use std::path::Path;
use std::process::Command;

fn drt() -> Command {
    Command::new(env!("CARGO_BIN_EXE_drt"))
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// cap1's `diluvium -e 'print("diluvium CLI runs: " .. _VERSION)'`: the
/// engine is present, assembled, and answers.
///
/// One difference worth pinning rather than smoothing over: upstream cap1
/// expects `Lua 5.5`, because it runs the plain CLI. The engine embedded
/// here answers `diluvium (lua) 5.5` — it names itself, which is the more
/// precise answer and the one a program checking what it is running under
/// should see.
#[test]
fn the_engine_is_present_and_answers() {
    let dir = tempfile::tempdir().unwrap();
    let prog = dir.path().join("version.dlua");
    write(&prog, r#"print("engine runs: " .. _VERSION)"#);

    let out = drt().arg("run").arg(&prog).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("engine runs: diluvium (lua) 5.5"),
        "the embedded language answers, and names itself: {stdout}"
    );
}

/// The claim that replaces cap1's two-binary check: one binary carries the
/// whole runtime. No separate host is built, shipped, or looked for.
#[test]
fn one_binary_carries_the_runtime() {
    let out = drt().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    for verb in ["run", "start", "repl", "ps"] {
        assert!(
            help.contains(verb),
            "'{verb}' is on the one binary's menu: {help}"
        );
    }
    // The swarm and the connectors are in *this* process, not behind a
    // second executable the installer forgot: a program that makes a
    // hostcall gets an answer from this binary alone.
    let dir = tempfile::tempdir().unwrap();
    let prog = dir.path().join("call.dlua");
    write(
        &prog,
        r#"
        local calls = queue.declare("host/calls", { capacity = 2, exported = true, on_full = "reject" })
        local replies = queue.declare("host/replies", { capacity = 2 })
        queue.push(calls, { tok = 1, call = "time" })
        local _, reply = queue.wait({replies})
        print("hostcall answered in-process: " .. reply.status)
        "#,
    );
    let out = drt().arg("run").arg(&prog).output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hostcall answered in-process: ok"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// cap1's blank slate. Upstream it is an empty database; here it is the
/// empty config, and the property is the same one: a runtime that starts
/// from nothing has nothing wired, and says so rather than guessing.
#[test]
fn the_empty_config_is_a_blank_slate_not_a_permissive_one() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("empty.json");
    write(&cfg, "{}");
    let prog = dir.path().join("probe.dlua");
    write(
        &prog,
        r#"
        local calls = queue.declare("host/calls", { capacity = 8, exported = true, on_full = "reject" })
        local replies = queue.declare("host/replies", { capacity = 8 })
        local function ask(call)
            queue.push(calls, { tok = 1, call = call })
            local _, r = queue.wait({replies})
            print(call .. ": " .. r.status)
        end
        ask("time")
        ask("fs/read")
        ask("sql/query")
        "#,
    );

    let out = drt()
        .arg("run")
        .arg(&prog)
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in ["time: denied", "fs/read: denied", "sql/query: denied"] {
        assert!(
            stdout.contains(line),
            "an empty config wires nothing, and every call is answered rather than dropped: \
             wanted '{line}' in {stdout}"
        );
    }
}

/// And the slate fills only when a config says so — the other half of
/// "blank", without which the first half proves nothing.
#[test]
fn the_slate_fills_only_when_the_config_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("time.json");
    write(
        &cfg,
        r#"{"caps": [{"capability": "host:time"}], "connectors": {"time": {}}}"#,
    );
    let prog = dir.path().join("probe.dlua");
    write(
        &prog,
        r#"
        local calls = queue.declare("host/calls", { capacity = 2, exported = true, on_full = "reject" })
        local replies = queue.declare("host/replies", { capacity = 2 })
        queue.push(calls, { tok = 1, call = "time" })
        local _, r = queue.wait({replies})
        print("time: " .. r.status)
        "#,
    );
    let out = drt()
        .arg("run")
        .arg(&prog)
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("time: ok"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
