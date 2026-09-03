//! The terminal contract, driven natively: the same `Term` a page drives,
//! over a memory filesystem, with the runtime's text captured through the
//! stdio sink. What a page adds is marshalling (`bindings`) and syscalls
//! (`wasi_shim`); everything a command *does* is exercised here.
//!
//! One thing is out of reach natively, deliberately: the C core's `print`
//! writes through libc's stdout, which in a page is `wasi_shim`'s
//! `fd_write` into the sink and natively is the process's own fd 1. So
//! these tests observe what programs *do* -- the files they write, the
//! statuses they exit with, the sentences the runtime says about them --
//! and the browser suite (`browser-test/`) is what proves `print` lands in
//! the terminal, by diffing the examples' `expected.txt` there.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use drt_platform::fs::Backend;
use drt_platform::stdio::{install_sink, uninstall_sink, Fd};
use drt_web::{Step, Term};

/// `Term::new` installs its filesystem as the process's, so two terminals
/// in two test threads would see each other's files. One at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

type Captured = Rc<RefCell<Vec<(Fd, Vec<u8>)>>>;

fn capture() -> Captured {
    let seen: Captured = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    install_sink(Box::new(move |fd, bytes| {
        log.borrow_mut().push((fd, bytes.to_vec()))
    }));
    seen
}

fn text(seen: &Captured, fd: Fd) -> String {
    let bytes: Vec<u8> = seen
        .borrow()
        .iter()
        .filter(|(f, _)| *f == fd)
        .flat_map(|(_, b)| b.iter().copied())
        .collect();
    String::from_utf8_lossy(&bytes).to_string()
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn file(term: &Term, path: &str) -> String {
    let bytes = term
        .fs()
        .read(Path::new(path))
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    String::from_utf8_lossy(&bytes).to_string()
}

/// Tick and sleep as told, the way a page's timer would, until it is over.
fn run_to_exit(session: &mut drt_web::Session) -> i32 {
    for _ in 0..10_000 {
        match session.tick() {
            Step::Sleep(d) => std::thread::sleep(d.min(Duration::from_millis(5))),
            Step::Exit(status) => return status,
            Step::Input { .. } => panic!("a run asked for input"),
        }
    }
    panic!("the session never ended")
}

#[test]
fn a_program_runs_from_the_seeded_filesystem_and_its_verdict_reaches_the_sink() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seen = capture();
    let term = Term::new();
    term.fs().add_file(
        "/work/ok.dlua",
        "assert(1 + 1 == 2) assert(host.time() > 0)",
    );
    term.fs()
        .add_file("/work/bad.dlua", "error('boom from the page')");
    term.fs().set_cwd("/work");
    let mut session = term.exec(&argv(&["drt", "run", "ok.dlua"]));
    assert_eq!(run_to_exit(&mut session), 0);
    assert!(session.is_over());
    assert_eq!(text(&seen, Fd::Stderr), "");
    let mut session = term.exec(&argv(&["drt", "run", "bad.dlua"]));
    assert_eq!(run_to_exit(&mut session), 1);
    let err = text(&seen, Fd::Stderr);
    assert!(err.starts_with("drt run: "), "{err}");
    assert!(err.contains("boom from the page"), "{err}");
    uninstall_sink();
}

#[test]
fn a_config_grants_a_directory_the_page_seeded_and_the_jail_holds() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seen = capture();
    let term = Term::new();
    term.fs()
        .add_file("/app/workspace/note.txt", "hello from the page");
    // What the program learns, it writes back into the granted directory,
    // where the page (and this test) can read it.
    term.fs().add_file(
        "/app/app.dlua",
        "local note = host.fs.read('note.txt')\n\
         local _, escape, why = host.fs.try_read('../app.dlua')\n\
         local _, sql, denied = host.try('sql/query', {sql = 'select 1'})\n\
         host.fs.write('report.txt', table.concat({note, escape .. ': ' .. why, sql .. ': ' .. denied}, '\\n'))",
    );
    term.fs().add_file(
        "/app/deploy.json",
        r#"{"program": {"path": "app.dlua"},
            "caps": [{"capability": "host:fs/*"}],
            "connectors": {"fs": {"scope": {"scope": "./workspace", "access": "readwrite"}}}}"#,
    );
    term.fs().set_cwd("/app");
    let mut session = term.exec(&argv(&["drt", "run", "--config", "deploy.json"]));
    assert_eq!(run_to_exit(&mut session), 0);
    assert_eq!(text(&seen, Fd::Stderr), "");
    let report = file(&term, "/app/workspace/report.txt");
    assert!(report.starts_with("hello from the page\n"), "{report}");
    assert!(
        report.contains("error: '../app.dlua' resolves outside the granted scope"),
        "{report}"
    );
    assert!(report.contains("denied: "), "{report}");
    uninstall_sink();
}

#[test]
fn the_repl_asks_for_lines_and_answers_them() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seen = capture();
    let term = Term::new();
    let mut session = term.exec(&argv(&["drt", "repl"]));
    assert_eq!(session.tick(), Step::Input { continuing: false });
    assert!(session.feed("x = 20").unwrap());
    assert_eq!(session.tick(), Step::Input { continuing: false });
    assert!(session.feed("for i = 1, 2 do").unwrap());
    assert_eq!(session.tick(), Step::Input { continuing: true });
    assert!(session.continuing());
    assert!(session.feed("x = x + 1 end").unwrap());
    assert_eq!(session.tick(), Step::Input { continuing: false });
    assert!(!session.feed("").unwrap(), "a blank line is not sent");
    assert!(session.feed("x").unwrap());
    assert_eq!(session.tick(), Step::Input { continuing: false });
    assert!(session.feed("nope()").unwrap());
    assert_eq!(session.tick(), Step::Input { continuing: false });
    assert_eq!(text(&seen, Fd::Stdout), "22\n");
    let err = text(&seen, Fd::Stderr);
    assert!(err.starts_with("drt repl — ^D to leave\n"), "{err}");
    assert!(err.contains("nil value"), "{err}");
    assert!(!session.is_over());
    uninstall_sink();
}

#[test]
fn a_deployment_drains_and_the_swarm_runs_in_the_page() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seen = capture();
    let term = Term::new();
    term.fs().add_dir("/d/workspace");
    // The parent spawns a child and records how the child ended, which is
    // the swarm's lifecycle queue answering inside a page.
    term.fs().add_file(
        "/d/app.json",
        r#"{"program": {"source": "local sys = queue.declare('system/lifecycle', {capacity = 4})\nlocal ev = queue.declare('system/events', {capacity = 16})\nassert(queue.push(sys, {op = 'spawn', code = 'local x = 1', caps = {}}))\nlocal seen = {}\nrepeat local _, e = queue.wait({ev}) seen[#seen + 1] = tostring(e.event) until e.event == 'exited'\nhost.fs.write('events.txt', table.concat(seen, ','))"},
            "caps": [{"capability": "lifecycle"}, {"capability": "queue:*"}, {"capability": "host:fs/*"}],
            "connectors": {"fs": {"scope": {"scope": "./workspace", "access": "readwrite"}}}}"#,
    );
    term.fs().set_cwd("/d");
    let mut session = term.exec(&argv(&["drt", "start", "--config", "app.json"]));
    assert_eq!(run_to_exit(&mut session), 0);
    assert_eq!(text(&seen, Fd::Stderr), "");
    assert_eq!(file(&term, "/d/workspace/events.txt"), "spawned,exited");
    uninstall_sink();
}

#[test]
fn the_command_line_is_the_binarys_own() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let seen = capture();
    let term = Term::new();
    assert_eq!(
        term.exec(&argv(&["drt", "--version"])).tick(),
        Step::Exit(0)
    );
    assert_eq!(
        text(&seen, Fd::Stdout),
        format!("drt {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        term.exec(&argv(&["drt", "frobnicate"])).tick(),
        Step::Exit(2)
    );
    assert!(text(&seen, Fd::Stderr).contains("unrecognized subcommand"));
    assert_eq!(
        term.exec(&argv(&["drt", "buildinfo"])).tick(),
        Step::Exit(0)
    );
    assert!(text(&seen, Fd::Stdout).contains("profile: "));
    assert_eq!(
        term.exec(&argv(&["drt", "run", "missing.dlua"])).tick(),
        Step::Exit(1)
    );
    assert!(text(&seen, Fd::Stderr).contains("drt run: cannot read missing.dlua"));
    // Help goes to stdout, as clap and the binary have it.
    assert_eq!(term.exec(&argv(&["drt", "--help"])).tick(), Step::Exit(0));
    assert!(text(&seen, Fd::Stdout).contains("The Diluvium RunTime"));
    uninstall_sink();
}
