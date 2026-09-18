//! `drt repl` end to end, through the real binary. A REPL that a pipe can
//! drive is a REPL a browser can drive: the whole contract is lines in,
//! text out, which is why the tests are pipes.

use std::io::Write;
use std::process::{Command, Stdio};

fn repl(input: &str, args: &[&str]) -> (String, String) {
    drive(input, args, &[])
}

/// The same, with `--unsafe` after the verb: global options come before
/// `repl` and the verb's own after it, which is where clap puts them.
fn unsealed(input: &str, args: &[&str]) -> (String, String) {
    drive(input, args, &["--unsafe"])
}

fn drive(input: &str, args: &[&str], verb_args: &[&str]) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_drt"))
        .args(args)
        .arg("repl")
        .args(verb_args)
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
                 "connectors": {{"fs": {{"scope": {{"scope": "{}", "access": "read"}}}}}}}}"#,
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

/// `--unsafe` puts the stdlib back and takes nothing else off.
///
/// The seal `drt run` keeps is what the flag lifts: `os`, `io` and
/// `require` are the libraries a *language* REPL is expected to have and
/// a sealed guest is not given. What it must not lift is the sandbox, so
/// the second half of this is the same ceiling test above, run again with
/// the flag on.
#[test]
fn unsafe_is_the_stdlib_seal_and_not_the_sandbox() {
    // Sealed: not there, and the banner does not claim otherwise.
    //
    // `"nil"` quoted, because `type()` returns a *string*. This line used
    // to read `nil` and be indistinguishable from the value `nil` — which
    // is the confusion quoting exists to remove, demonstrated by the one
    // test that happened to trip over it.
    let (out, err) = repl("type(os)\ntype(io)\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec![r#""nil""#, r#""nil""#],
        "{out}"
    );
    assert!(err.starts_with("drt repl — ^D to leave"), "{err}");

    // Unsealed: there, and the banner says so rather than looking the
    // same as a sealed one, which would be a trap.
    let (out, err) = unsealed("type(os)\ntype(io)\nos.time() > 0\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec![r#""table""#, r#""table""#, "true"],
        "{out}{err}"
    );
    assert!(
        err.contains("unsafe stdlib: os, io, require"),
        "the banner names what is off: {err}"
    );
}

/// The capability ceiling holds with the flag on: an unwired connector is
/// denied exactly as it is without it. The stdlib and the caps are two
/// different seals, and only one of them has a flag.
#[test]
fn unsafe_does_not_widen_the_capability_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::write(work.join("note.txt"), "reachable").unwrap();
    let config = dir.path().join("drt.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"caps": [{{"capability": "host:fs/*"}}],
                 "connectors": {{"fs": {{"scope": {{"scope": "{}", "access": "read"}}}}}}}}"#,
            work.display()
        ),
    )
    .unwrap();
    let cfg = config.display().to_string();

    let (out, err) = unsealed(
        "host.fs.read('note.txt')\nhost.fs.read('../escape.txt')\n1 + 1\n",
        &["--config", &cfg],
    );
    assert!(out.contains("reachable"), "inside the scope: {out}{err}");
    assert!(
        !out.contains("escape"),
        "outside it, still refused: {out}{err}"
    );
    assert!(out.contains('2'), "and the repl goes on: {out}");
}

/// A table prints as its contents. Every table used to print as
/// `table: 0x...`, `host` included — which made the Tab completion built
/// so `host.<Tab>` works into half a feature, since asking what `host`
/// *was* answered with an address.
#[test]
fn a_table_prints_as_its_contents() {
    let (out, _) = repl("{1,2,3}\n{a=1, b={c=2}}\n{}\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec!["{1, 2, 3}", "{a = 1, b = {c = 2}}", "{}"],
        "{out}"
    );
}

/// The array part keeps its order and the rest is sorted. Lua's own key
/// order is unspecified, so without the sort the same table could print
/// two ways and a reader could not tell whether it had changed.
#[test]
fn keys_are_ordered_so_one_table_prints_one_way() {
    let (out, _) = repl("{10, 20, zed=1, alpha=2, [3.5]='x'}\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec![r#"{10, 20, [3.5] = "x", alpha = 2, zed = 1}"#],
        "{out}"
    );
}

/// A table that holds itself is a cycle, not an infinite line. Rendering
/// is bounded three ways and this is the one a REPL meets by accident.
#[test]
fn a_cycle_is_named_rather_than_followed() {
    let (out, _) = repl("local t = {} t.self = t return t\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec!["{self = <cycle>}"],
        "{out}"
    );
}

/// Functions and userdata show their kind, not their address. An address
/// is noise to a reader and changes every run, which would make any test
/// of this output a test of the allocator.
#[test]
fn a_function_shows_its_kind_and_not_its_address() {
    let (out, _) = repl("{f = print}\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec!["{f = <function>}"],
        "{out}"
    );
    assert!(!out.contains("0x"), "an address reached the output: {out}");
}

/// `host` is the value this was built for: the whole wired surface, one
/// dot deep, rather than an address.
#[test]
fn host_shows_what_it_carries() {
    let (out, _) = repl("host\n", &[]);
    assert!(out.contains("fs = {"), "no fs in {out}");
    assert!(out.contains("read = <function>"), "no fs.read in {out}");
    assert!(
        !out.contains("table: 0x"),
        "an address reached the output: {out}"
    );
}

/// An error inside a function carries the frames that led to it. The
/// message alone says `repl:1:` and nothing about `g`, which is the case
/// a stack exists for.
#[test]
fn an_error_inside_a_function_carries_its_stack() {
    let (_, err) = repl(
        "function f() return g() end\nfunction g() error('deep') end\nf()\n",
        &[],
    );
    assert!(err.contains("deep"), "{err}");
    assert!(err.contains("stack traceback:"), "no stack in: {err}");
    assert!(
        !err.contains("xpcall"),
        "the repl's own frames reached the user: {err}"
    );
    assert!(
        !err.contains("in main chunk"),
        "the repl's own main chunk reached the user: {err}"
    );
}

/// An error raised at the prompt does not. Its message already says
/// `repl:1:`, so frames under it would be three lines repeating what the
/// first one said.
#[test]
fn an_error_at_the_prompt_is_one_line() {
    let (_, err) = repl("error('boom')\n", &[]);
    assert!(err.contains("boom"), "{err}");
    assert!(
        !err.contains("stack traceback:"),
        "a top-level error grew a stack: {err}"
    );
}

/// A string is quoted and a non-string is not, so the two are told apart.
///
/// This is the reason rendering quotes at all: without it the string
/// `"nil"` and the value `nil` printed the same three characters, and so
/// did `"42"` and `42`. It also keeps one value printing one way whether
/// or not it is inside a table, which a quote-outside-only rule would
/// break.
#[test]
fn a_string_is_quoted_so_it_is_not_its_own_value() {
    let (out, _) = repl("'nil'\nnil\n'42'\n42\n{'x'}\n", &[]);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        vec![r#""nil""#, "nil", r#""42""#, "42", r#"{"x"}"#],
        "{out}"
    );
}
