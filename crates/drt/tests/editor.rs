//! The line editor, over a terminal made of two buffers.
//!
//! `drt repl` on a tty runs one editor (doc/Wasm.md D8, M8) and so does a
//! page; what differs is only the backend under it. `MemTerminal` is a
//! third backend for the same loop, so the editing a person gets can be
//! asserted without a tty, a browser, or a screen-scrape — script the
//! keys a terminal would send, read back what was drawn.
//!
//! These are the behaviours §5 enumerates. They are here rather than in
//! `tests/repl.rs` because that file drives the binary through a pipe,
//! which is the path that deliberately has no editor.
#![cfg(feature = "cli")]

use std::sync::{Arc, Mutex};

use drt::repl::Repl;
use drt_caps::Grant;
use drt_connector::{Dispatcher, Registry};
use ego_cli::term::mem::MemTerminal;
use ego_cli::term::Size;

/// Everything the runtime printed, in order, with the fd it went to.
type Captured = Arc<Mutex<Vec<(drt_platform::stdio::Fd, Vec<u8>)>>>;

/// Drive one scripted session to end-of-input, and hand back what the
/// terminal drew and what the REPL answered.
///
/// The second is the REPL's own answers, not `print`: natively the C
/// core writes through libc to fd 1 and never passes the Rust sink, so
/// these scripts evaluate expressions and read the values back. The
/// browser suite is where `print` reaching a terminal is asserted.
fn session(keys: &str) -> (String, String) {
    let seen: Captured = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    drt_platform::stdio::install_sink(Box::new(move |fd, bytes| {
        log.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((fd, bytes.to_vec()))
    }));

    let mut repl = Repl::new(
        Arc::new(Dispatcher::new(Registry::new())),
        Vec::<Grant>::new(),
        drt_config::Budget::default(),
    )
    .expect("the repl program starts");

    let mut terminal = MemTerminal::raw(Size::new(80, 24));
    terminal.push_input(keys);
    let mut editor = drt::repl::editor(&repl, terminal);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(drt::repl::edit(&mut repl, &mut editor))
        .expect("the session ends cleanly");

    drt_platform::stdio::uninstall_sink();
    let printed: Vec<u8> = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    (
        editor.terminal().output().to_string(),
        String::from_utf8_lossy(&printed).to_string(),
    )
}

/// A line typed, edited, and accepted: the editor is between the keys and
/// the guest, and what the guest evaluates is what the editing produced.
#[test]
fn a_line_is_edited_before_the_guest_sees_it() {
    // A typo backspaced away: what the guest evaluates is `6 * 7`, and
    // never the `6 * 71` that was typed.
    let (drawn, answered) = session("6 * 71\u{7f}\r");
    assert_eq!(answered.trim(), "42", "answered: {answered:?}");
    // The prompt is the REPL's, and it was drawn.
    assert!(drawn.contains("dv> "), "drawn: {drawn:?}");
}

/// Up recalls, which is the single thing a REPL without history costs a
/// person most often.
#[test]
fn history_walks_back_to_the_previous_line() {
    // Evaluate `7 * 6`, then press Up and Enter to run it again.
    let (_, answered) = session("7 * 6\r\u{1b}[A\r");
    assert_eq!(
        answered.matches("42").count(),
        2,
        "the recalled line did not run again: {answered:?}"
    );
}

/// Tab's candidates come from the instance, not from a list the host
/// hard-coded: `host.tim` completes because the guest answered with the
/// names in its own scope (doc/Wasm.md §5).
#[test]
fn tab_completes_a_name_the_guest_answered_with() {
    let (drawn, answered) = session("host.tim\t\r");
    assert!(
        drawn.contains("host.time"),
        "Tab did not complete from the guest's names: {drawn:?}"
    );
    // And what it completed to is a real name: it evaluates to a function
    // rather than failing on a nil.
    assert!(
        answered.contains("function"),
        "the completed name did not evaluate: {answered:?}"
    );
}

/// A continuation is the guest's judgement, and the prompt follows it.
#[test]
fn an_unfinished_line_gets_the_continuation_prompt() {
    let (drawn, answered) = session("x = 0\rfor i = 1, 3 do\rx = x + i end\rx\r");
    assert!(drawn.contains(">> "), "no continuation prompt: {drawn:?}");
    assert_eq!(answered.trim(), "6", "answered: {answered:?}");
}

/// Ctrl+C abandons the line and whatever it was continuing, and the
/// session survives it.
#[test]
fn control_c_abandons_the_line_and_the_repl_goes_on() {
    let (_, answered) = session("for i = 1, 2 do\r\u{3}'after'\r");
    assert_eq!(
        answered.trim(),
        "after",
        "the repl did not survive ^C, or the abandoned line came back: {answered:?}"
    );
}
