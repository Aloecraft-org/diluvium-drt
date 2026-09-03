//! `exec/run` against real processes: the contract `dhost_exec.c` fixes,
//! reply by reply, plus the allow list DRT adds. Every test is a plain
//! `#[test]` under `pollster`, which is the caller every guest loop is
//! (doc/Failure-Modes.md FM-3): nothing here may need a reactor.

use std::time::{Duration, Instant};

use drt_caps::Scope;
use drt_connector::{CallResult, Connector};
use drt_connector_exec::ExecConnector;

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

// --- the answers -----------------------------------------------------------

#[test]
fn a_command_answers_status_stdout_and_stderr() {
    let v = call(None, args(&["sh", "-c", "echo out; echo err >&2"], vec![])).unwrap();
    assert_eq!(status(&v), 0);
    assert_eq!(text(&v, "stdout"), "out\n");
    assert_eq!(text(&v, "stderr"), "err\n");
}

#[test]
fn a_nonzero_exit_is_an_answer_not_an_error() {
    let v = call(None, args(&["sh", "-c", "exit 3"], vec![])).unwrap();
    assert_eq!(status(&v), 3);
    assert_eq!(text(&v, "stdout"), "");
}

#[test]
fn a_program_that_does_not_exist_is_127() {
    let v = call(None, args(&["drt-no-such-program-anywhere"], vec![])).unwrap();
    assert_eq!(status(&v), 127);
    assert_eq!(text(&v, "stdout"), "");
    assert_eq!(text(&v, "stderr"), "");
    // A path, too: the shell's convention does not depend on the spelling.
    let v = call(None, args(&["/nowhere/at/all"], vec![])).unwrap();
    assert_eq!(status(&v), 127);
}

#[test]
fn a_signal_death_is_128_plus_the_signal() {
    let v = call(None, args(&["sh", "-c", "kill -9 $$"], vec![])).unwrap();
    assert_eq!(status(&v), 128 + 9);
}

#[test]
fn stdin_reaches_the_child() {
    let v = call(
        None,
        args(&["cat"], vec![("stdin", rmpv::Value::from("fed in"))]),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    assert_eq!(text(&v, "stdout"), "fed in");
    // And as bin, which the guest's codec cannot tell from str.
    let v = call(
        None,
        args(
            &["cat"],
            vec![("stdin", rmpv::Value::Binary(b"raw".to_vec()))],
        ),
    )
    .unwrap();
    assert_eq!(text(&v, "stdout"), "raw");
}

#[test]
fn a_child_that_never_reads_a_large_stdin_still_answers() {
    // More than a pipe buffer holds, to a program that closes stdin
    // unread: the feeder must not block the deadline.
    let big = "x".repeat(256 * 1024);
    let v = call(
        Some(map(vec![(
            "max_output_bytes",
            rmpv::Value::from(1_000_000u64),
        )])),
        args(
            &["sh", "-c", "exec 0<&-; echo ignored"],
            vec![("stdin", rmpv::Value::from(big))],
        ),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    assert_eq!(text(&v, "stdout"), "ignored\n");
}

#[test]
fn cwd_is_where_the_child_runs() {
    let dir = tempfile::tempdir().unwrap();
    let v = call(
        None,
        args(
            &["sh", "-c", "pwd -P"],
            vec![("cwd", rmpv::Value::from(dir.path().to_str().unwrap()))],
        ),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    let expected = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(text(&v, "stdout").trim_end(), expected.to_str().unwrap());
    // A cwd that is not there fails the way an absent program does.
    let v = call(
        None,
        args(
            &["sh", "-c", "pwd"],
            vec![("cwd", rmpv::Value::from("/nowhere/at/all"))],
        ),
    )
    .unwrap();
    assert_eq!(status(&v), 127);
}

#[test]
fn output_that_is_not_text_travels_as_bytes() {
    let v = call(None, args(&["sh", "-c", "printf '\\377\\376'"], vec![])).unwrap();
    assert_eq!(field(&v, "stdout"), &rmpv::Value::Binary(vec![0xff, 0xfe]));
    // Text stays text, byte for byte what the C host emits.
    let v = call(None, args(&["echo", "plain"], vec![])).unwrap();
    assert!(matches!(field(&v, "stdout"), rmpv::Value::String(_)));
}

// --- the bounds ------------------------------------------------------------

#[test]
fn the_deadline_kills_the_child_and_answers_error() {
    let started = Instant::now();
    let err = call(
        None,
        args(
            &["sleep", "5"],
            vec![("timeout_ms", rmpv::Value::from(100u64))],
        ),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the child was killed at the 100 ms deadline"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the kill was prompt"
    );
}

#[test]
fn a_call_may_ask_for_less_than_the_ceiling_never_more() {
    let scope = map(vec![("max_timeout_ms", rmpv::Value::from(500u64))]);
    let err = call(
        Some(scope.clone()),
        args(&["true"], vec![("timeout_ms", rmpv::Value::from(501u64))]),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "timeout_ms passed the host's ceiling (500 ms); a call may ask for less, never more"
    );
    let v = call(
        Some(scope),
        args(&["true"], vec![("timeout_ms", rmpv::Value::from(500u64))]),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    for bad in [
        rmpv::Value::from(0u64),
        rmpv::Value::from(-1i64),
        rmpv::Value::from("soon"),
    ] {
        let err = call(None, args(&["true"], vec![("timeout_ms", bad)])).unwrap_err();
        assert_eq!(err.to_string(), "timeout_ms must be a positive integer");
    }
}

#[test]
fn the_output_cap_kills_the_child_and_refuses_the_output() {
    let scope = map(vec![("max_output_bytes", rmpv::Value::from(1024u64))]);
    let started = Instant::now();
    let err = call(Some(scope.clone()), args(&["yes"], vec![])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "stdout passed this deployment's byte cap (1024); the child was killed, the output \
         refused"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the kill was prompt"
    );
    // stderr has its own cap, and its own sentence.
    let err = call(Some(scope), args(&["sh", "-c", "yes >&2"], vec![])).unwrap_err();
    assert!(err
        .to_string()
        .starts_with("stderr passed this deployment's byte cap (1024)"));
}

#[test]
fn stdin_is_under_the_same_cap() {
    let scope = map(vec![("max_output_bytes", rmpv::Value::from(8u64))]);
    let err = call(
        Some(scope),
        args(&["cat"], vec![("stdin", rmpv::Value::from("nine bytes"))]),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "stdin is bigger than this deployment's byte cap (8)"
    );
}

#[test]
fn nothing_the_child_starts_outlives_the_call() {
    // The child exits at once, leaving a background subshell that holds
    // its stdout and would write a marker two seconds later. The answer is
    // the child's own exit with what it printed; the marker never appears,
    // because the group is swept on the way out.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("late");
    let started = Instant::now();
    let v = call(
        None,
        args(
            &[
                "sh",
                "-c",
                "(sleep 2; echo late > \"$1\") & echo started",
                "sh",
                marker.to_str().unwrap(),
            ],
            vec![],
        ),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    assert_eq!(text(&v, "stdout"), "started\n");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "answered at the exit, not at EOF"
    );
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !marker.exists(),
        "the background subshell was killed with its group"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_descendant_that_escapes_the_group_cannot_hold_the_answer() {
    // `setsid` leaves the group, so the sweep cannot reach it and it keeps
    // the pipe open. The answer is what was buffered at exit, promptly,
    // which is the C host's behaviour too.
    let started = Instant::now();
    let v = call(
        None,
        args(&["sh", "-c", "setsid sleep 3 & echo started"], vec![]),
    )
    .unwrap();
    assert_eq!(status(&v), 0);
    assert_eq!(text(&v, "stdout"), "started\n");
    assert!(started.elapsed() < Duration::from_secs(1));
}

// --- the request -----------------------------------------------------------

#[test]
fn argv_is_a_vector_and_nothing_else() {
    let vector = "args.argv must be a non-empty array of strings (a vector, not a shell string)";
    let err = pollster::block_on(ExecConnector::new().call("exec/run", None, None)).unwrap_err();
    assert_eq!(err.to_string(), "args.argv must be the command vector");
    let err = call(None, map(vec![("argv", rmpv::Value::from("ls -l"))])).unwrap_err();
    assert_eq!(err.to_string(), vector);
    let err = call(None, map(vec![("argv", strings(&[]))])).unwrap_err();
    assert_eq!(err.to_string(), vector);
    let err = call(
        None,
        map(vec![(
            "argv",
            rmpv::Value::Array(vec![rmpv::Value::from("echo"), rmpv::Value::from(7u64)]),
        )]),
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "argv[2] is not a plain string");
    let sixty_five: Vec<&str> = std::iter::repeat_n("x", 65).collect();
    let err = call(None, args(&sixty_five, vec![])).unwrap_err();
    assert_eq!(err.to_string(), "argv is longer than the host's bound (64)");
}

#[test]
fn the_connector_answers_one_call() {
    let err = pollster::block_on(ExecConnector::new().call("exec/spawn", None, None)).unwrap_err();
    assert_eq!(
        err.to_string(),
        "the exec connector answers 'exec/run'; 'exec/spawn' is not it"
    );
}

// --- the scope -------------------------------------------------------------

fn validate(scope: Option<rmpv::Value>) -> Result<(), String> {
    let scope = scope.map(Scope);
    ExecConnector::new().scope_type().validate(scope.as_ref())
}

#[test]
fn the_scope_is_optional_and_its_keys_are_the_c_hosts() {
    assert_eq!(validate(None), Ok(()));
    assert_eq!(
        validate(Some(map(vec![
            ("max_timeout_ms", rmpv::Value::from(10_000u64)),
            ("max_output_bytes", rmpv::Value::from(1_048_576u64)),
        ]))),
        Ok(())
    );
    // A typo is refused by name at startup, as the C host refuses it.
    let err = validate(Some(map(vec![("max_timeout", rmpv::Value::from(1u64))]))).unwrap_err();
    assert!(err.contains("max_timeout"), "{err}");
    assert!(
        validate(Some(map(vec![("max_timeout_ms", rmpv::Value::from(0u64))])))
            .unwrap_err()
            .contains("refuse every call")
    );
    assert!(validate(Some(rmpv::Value::from("loose"))).is_err());
}

#[test]
fn the_allow_list_is_checked_at_startup_by_name() {
    let err = validate(Some(map(vec![("allow", strings(&["sh"]))]))).unwrap_err();
    assert_eq!(err, "allow entries are absolute paths; 'sh' is not");
    let err = validate(Some(map(vec![("allow", strings(&["/nowhere/at/all"]))]))).unwrap_err();
    assert!(
        err.starts_with("allow names '/nowhere/at/all', which cannot be resolved"),
        "{err}"
    );
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("not-executable");
    std::fs::write(&plain, "text").unwrap();
    let err = validate(Some(map(vec![(
        "allow",
        strings(&[plain.to_str().unwrap()]),
    )])))
    .unwrap_err();
    assert!(err.ends_with("is not an executable file"), "{err}");
    assert_eq!(
        validate(Some(map(vec![("allow", strings(&["/bin/sh"]))]))),
        Ok(())
    );
}

#[test]
fn the_allow_list_decides_which_programs_a_call_may_start() {
    let scope = map(vec![("allow", strings(&["/bin/sh"]))]);
    // By name, found on PATH and compared after symlinks are resolved.
    let v = call(
        Some(scope.clone()),
        args(&["sh", "-c", "echo via sh"], vec![]),
    )
    .unwrap();
    assert_eq!(text(&v, "stdout"), "via sh\n");
    // By path, the same.
    let v = call(
        Some(scope.clone()),
        args(&["/bin/sh", "-c", "exit 4"], vec![]),
    )
    .unwrap();
    assert_eq!(status(&v), 4);
    // Anything else is refused by name, and never started.
    let err = call(Some(scope.clone()), args(&["ls", "/"], vec![])).unwrap_err();
    assert_eq!(
        err.to_string(),
        "'ls' is outside this scope's allow list; a program may start only what the \
         deployment allows"
    );
    // A program that is not there at all is still the shell's 127.
    let v = call(Some(scope), args(&["drt-no-such-program-anywhere"], vec![])).unwrap();
    assert_eq!(status(&v), 127);
}
