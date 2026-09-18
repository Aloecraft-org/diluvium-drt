//! The part of `exec/run`'s contract that is the same on every platform,
//! run on every platform.
//!
//! `run.rs` is the contract in unix's own vocabulary: `sh -c`, `setsid`, a
//! signal death reported as `128 + signo`. Those tests are not weaker for
//! being unix's -- they check things unix genuinely does -- but they cannot
//! run where there is no `/bin/sh`, and for as long as they were the only
//! tests the Windows build of this connector was covered by nothing.
//!
//! So: the same answers, asked through whatever shell the host has. What is
//! tested here is what the header of `lib.rs` promises on both -- a status,
//! a nonzero exit as an answer, 127 for a program that is not there, the
//! deadline, the byte cap, the allow list -- plus the one refusal that
//! exists only on Windows, that `argv` which is not UTF-8 is named rather
//! than guessed at.

use std::time::{Duration, Instant};

use drt_caps::Scope;
use drt_connector::{CallResult, Connector};
use drt_connector_exec::ExecConnector;

// --- the harness, as `run.rs` spells it ------------------------------------

fn map(pairs: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    rmpv::Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (rmpv::Value::from(k), v))
            .collect(),
    )
}

fn strings(items: &[&str]) -> rmpv::Value {
    rmpv::Value::Array(items.iter().map(|s| rmpv::Value::from(*s)).collect())
}

fn args(argv: &[&str], extra: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    let mut pairs = vec![("argv", strings(argv))];
    pairs.extend(extra);
    map(pairs)
}

fn call(scope: Option<rmpv::Value>, args: rmpv::Value) -> CallResult {
    let scope = scope.map(Scope);
    pollster::block_on(ExecConnector::new().call("exec/run", Some(args), scope.as_ref()))
}

fn field<'a>(value: &'a rmpv::Value, name: &str) -> &'a rmpv::Value {
    value
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no field '{name}' in {value}"))
}

fn status(value: &rmpv::Value) -> i64 {
    field(value, "status").as_i64().unwrap()
}

fn text(value: &rmpv::Value, name: &str) -> String {
    match field(value, name) {
        rmpv::Value::String(s) => s.as_str().unwrap().to_string(),
        rmpv::Value::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("{name} is neither str nor bin: {other}"),
    }
}

// --- the same intent, spelled for the host ---------------------------------

/// The host's shell, and the flag that means "run this line".
///
/// Named here once so a test below reads as its intent and not as a
/// platform. `cmd.exe` is to Windows what `/bin/sh` is to unix: present on
/// every install, which is the only property these tests need of it.
#[cfg(unix)]
const SHELL: [&str; 2] = ["/bin/sh", "-c"];
#[cfg(windows)]
const SHELL: [&str; 2] = ["cmd.exe", "/c"];

/// A line that writes `out` to stdout and exits cleanly.
#[cfg(unix)]
const SAYS_OUT: &str = "echo out";
#[cfg(windows)]
const SAYS_OUT: &str = "echo out";

/// A line that exits with status 3 and says nothing.
#[cfg(unix)]
const EXITS_THREE: &str = "exit 3";
#[cfg(windows)]
const EXITS_THREE: &str = "exit 3";

/// A line that runs far longer than any deadline a test sets.
#[cfg(unix)]
const RUNS_LONG: &str = "sleep 30";
#[cfg(windows)]
const RUNS_LONG: &str = "ping -n 31 127.0.0.1 > NUL";

/// A line that writes far more than any cap a test sets.
#[cfg(unix)]
const SPEWS: &str = "yes drt-spews-a-lot";
#[cfg(windows)]
const SPEWS: &str = "for /L %i in (1,1,200000) do @echo drt-spews-a-lot";

/// A program that copies stdin to stdout. `sort` is not the obvious
/// choice, and is the portable one: unix has it and so does Windows, and
/// for a single line of input its output is that line.
const ECHOES_STDIN: &str = "sort";

fn shell(line: &str) -> Vec<String> {
    vec![SHELL[0].to_string(), SHELL[1].to_string(), line.to_string()]
}

fn shell_args(line: &str, extra: Vec<(&str, rmpv::Value)>) -> rmpv::Value {
    let argv = shell(line);
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    args(&borrowed, extra)
}

// --- the answers -----------------------------------------------------------

/// The ordinary reply: a status, and whatever the program wrote.
#[test]
fn a_command_answers_status_and_stdout() {
    let v = call(None, shell_args(SAYS_OUT, vec![])).unwrap();
    assert_eq!(status(&v), 0);
    assert!(
        text(&v, "stdout").contains("out"),
        "stdout was {:?}",
        text(&v, "stdout")
    );
}

/// A nonzero exit is the child's answer, read the way a shell script reads
/// `$?`, and never an error.
#[test]
fn a_nonzero_exit_is_an_answer_not_an_error() {
    let v = call(None, shell_args(EXITS_THREE, vec![])).unwrap();
    assert_eq!(status(&v), 3);
}

/// The shell's own convention, and the one number in this connector that
/// a guest is most likely to branch on.
#[test]
fn a_program_that_does_not_exist_is_127() {
    let v = call(None, args(&["drt-no-such-program-anywhere-at-all"], vec![])).unwrap();
    assert_eq!(status(&v), 127);
}

/// Stdin reaches the child, and what the child writes comes back.
#[test]
fn stdin_reaches_the_child() {
    let v = call(
        None,
        args(
            &[ECHOES_STDIN],
            vec![("stdin", rmpv::Value::from("fed in"))],
        ),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    assert!(
        text(&v, "stdout").contains("fed in"),
        "stdout was {:?}",
        text(&v, "stdout")
    );
}

/// The deadline fires, the child is killed, and the call answers `error`
/// -- and it does so near the deadline rather than near the child's own
/// runtime, which is the half that a kill aimed at the wrong thing fails.
#[test]
fn the_deadline_kills_the_child_and_answers_error() {
    let started = Instant::now();
    let err = call(
        None,
        shell_args(RUNS_LONG, vec![("timeout_ms", rmpv::Value::from(400u64))]),
    )
    .unwrap_err();
    let waited = started.elapsed();
    assert!(
        err.to_string().contains("deadline"),
        "the refusal did not name the deadline: {err}"
    );
    assert!(
        waited < Duration::from_secs(20),
        "the call outlived its deadline by {waited:?}, so the kill missed"
    );
}

/// Past the cap the child is killed and the output refused, rather than
/// truncated -- the same rule as every other cap in this tree.
#[test]
fn the_output_cap_kills_the_child_and_refuses_the_output() {
    let scope = map(vec![
        ("max_output_bytes", rmpv::Value::from(4096u64)),
        ("max_timeout_ms", rmpv::Value::from(20_000u64)),
    ]);
    let err = call(Some(scope), shell_args(SPEWS, vec![])).unwrap_err();
    assert!(
        err.to_string().contains("byte cap"),
        "the refusal did not name the cap: {err}"
    );
}

/// A call may ask for less than the ceiling, never more, and the refusal
/// says which ceiling and in whose words.
#[test]
fn a_call_may_ask_for_less_than_the_ceiling_never_more() {
    let scope = map(vec![("max_timeout_ms", rmpv::Value::from(500u64))]);
    let err = call(
        Some(scope),
        shell_args(SAYS_OUT, vec![("timeout_ms", rmpv::Value::from(501u64))]),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("ceiling"),
        "the refusal did not name the ceiling: {err}"
    );
}

/// A program the allow list does not name does not run. Which of the two
/// honest answers it gets depends on where the name fails first.
///
/// A bare name is looked up on `PATH` so the list has a resolved path to
/// compare, and a name that is nowhere on `PATH` never reaches the
/// comparison: that is `{status = 127}`, the same answer the C host gives
/// for a program that is not there, and not a lecture about a list the
/// call never got to. A name that *does* resolve, and is not on the list,
/// is the refusal proper. Both are asserted, because which one a given
/// host gives depends on what is installed on it -- and the thing that
/// must never happen, on either, is that it runs.
#[test]
fn the_allow_list_decides_which_programs_a_call_may_start() {
    let scope = map(vec![("allow", strings(&[SHELL[0]]))]);

    // Nowhere on PATH: 127, before the list is consulted.
    let answer = call(
        Some(scope.clone()),
        args(&["drt-not-on-the-allow-list"], vec![]),
    );
    match answer {
        Ok(v) => assert_eq!(status(&v), 127, "an unlisted program answered {v}"),
        Err(e) => assert!(
            e.to_string().contains("allow list"),
            "an unlisted program was refused for the wrong reason: {e}"
        ),
    }

    // On the list: it runs, which is the other half of the same rule.
    let v = call(Some(scope), shell_args(SAYS_OUT, vec![])).unwrap();
    assert_eq!(status(&v), 0, "the allowed shell did not run");
}

/// `argv` entries are checked before anything is started, so a guest's
/// mistake is a sentence and not a process.
#[test]
fn argv_is_a_vector_and_nothing_else() {
    let err = call(None, map(vec![("argv", rmpv::Value::from("ls -l"))])).unwrap_err();
    assert!(
        err.to_string().contains("vector, not a shell string"),
        "{err}"
    );
}

/// Windows passes a program UTF-16, so bytes that are not UTF-8 cannot be
/// handed over. Refused by name rather than lossily converted: a
/// replacement character in a path is a different path.
#[cfg(windows)]
#[test]
fn argv_that_is_not_utf8_is_refused_by_name() {
    let argv = rmpv::Value::Array(vec![rmpv::Value::Binary(vec![0xff, 0xfe])]);
    let err = call(None, map(vec![("argv", argv)])).unwrap_err();
    assert!(
        err.to_string().contains("UTF-8"),
        "the refusal did not name the encoding: {err}"
    );
}
