//! `drt repl` end to end, through the real binary. A REPL that a pipe can
//! drive is a REPL a browser can drive: the whole contract is lines in,
//! text out, which is why the tests are pipes.

use std::io::Write;
use std::process::{Command, Stdio};

fn repl(input: &str, args: &[&str]) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_drt"))
        .args(args)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The three things a REPL is for: an expression prints its value, a
/// statement does not print, and state survives the line that made it.
#[test]
fn expressions_print_statements_do_not_and_state_survives() {
    let (out, _) = repl("1 + 1\nx = 40\nx + 2\n", &[]);
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines, vec!["2", "42"], "{out}");
}

/// Prompts and errors go to stderr, answers to stdout — so `drt repl <
/// script > out` yields the answers alone, and a pipeline is a usable way
/// to drive it.
#[test]
fn answers_are_stdout_and_everything_else_is_stderr() {
    let (out, err) = repl("nope()\n7\n", &[]);
    assert_eq!(out.lines().collect::<Vec<_>>(), vec!["7"], "{out}");
    assert!(err.contains("nil value"), "{err}");
    assert!(err.contains("dv>"), "the prompt belongs on stderr: {err}");
}

/// A line that runs off the end of the input is unfinished, not wrong: the
/// REPL asks for more instead of reporting a syntax error the user has not
/// finished making.
#[test]
fn an_unfinished_line_is_continued_rather_than_refused() {
    let (out, err) = repl("for i = 1, 3 do\n  print(i * 10)\nend\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec!["10", "20", "30"],
        "{out}"
    );
    assert!(err.contains(">>"), "a continuation prompt: {err}");
    assert!(!err.contains("<eof>"), "not reported as an error: {err}");
}

/// The REPL is an instance under the config's ceiling, not a way around
/// it: a wired connector answers, and an unwired one is denied — the same
/// answer the same call gets from any other guest.
#[test]
fn the_repl_is_an_instance_under_the_configs_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("note.txt"), "reachable").unwrap();
    let config = dir.path().join("drt.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"caps": [{{"capability": "host:fs/*"}}],
                 "connectors": {{"fs": {{"scope": {{"scope": "{}", "access": "readonly"}}}}}}}}"#,
            work.display()
        ),
    )
    .unwrap();
    let cfg = config.display().to_string();

    // Inside the granted place: the file reads.
    let (out, _) = repl("host.fs.read('note.txt')\n", &["--config", &cfg]);
    assert!(out.contains("reachable"), "{out}");

    // Outside it: refused, and the REPL keeps going.
    let (out, err) = repl(
        "host.fs.read('../escape.txt')\n1 + 1\n",
        &["--config", &cfg],
    );
    assert!(!out.contains("escape"), "{out}{err}");
    assert!(out.contains('2'), "the repl survives a refusal: {out}");
}
