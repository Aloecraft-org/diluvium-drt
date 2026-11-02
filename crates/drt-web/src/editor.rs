//! The line editor in the page (doc/Wasm.md D8, M8, §5).
//!
//! The same `ego_cli::Session` a tty gets, over `XtermTerminal` instead of
//! a crossterm backend, and completing by `drt::repl::Names` -- the same
//! type, not a second implementation of the same rule. What the page keeps
//! is the loop: §5 puts exactly one `read_line()` in a host, at the point
//! the driver parks on input, and everywhere else `DrtSession::tick` still
//! decides. So this exposes a line at a time rather than a loop, and the
//! `.await` that owns the wait is a JS microtask.
//!
//! ## surface block
//!
//! - [`Editor::attach`]: take the page's xterm.js object.
//! - [`Editor::read_line`]: one line, with history, motions, undo and Tab,
//!   coloured by `drt::repl::Syntax` when the host says the line is code
//!   (a `dv> ` line is; a `$ ` line is not, and `--config` would read as
//!   a comment).
//! - [`Editor::set_candidates`]: what Tab serves from, refreshed by the
//!   host after every accepted line (§5: a snapshot, because `Completer`
//!   is synchronous and reaching the guest is a round trip).
//! - [`Outcome`]: what a read ended as.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use drt::repl::{Names, Syntax};
use ego_cli::extend::Highlighter;
use ego_cli::term::browser::XtermTerminal;
use ego_cli::{ReadOutcome, Session};
use wasm_bindgen::prelude::*;

/// How a [`Editor::read_line`] ended.
pub enum Outcome {
    /// The line, without its newline.
    Line(String),
    /// Ctrl+C: abandoned, and the editor is still good.
    Interrupted,
    /// Ctrl+D on an empty line, or the terminal went away.
    Eof,
}

/// An `ego_cli` session over the page's terminal.
///
/// The session is behind `Rc<RefCell<Option<_>>>` because a read is a JS
/// promise: the future outlives the call that made it, and JS is the only
/// thing that could ask for a second one while the first is still running.
/// A read *takes* the session and puts it back when it ends, so the cell is
/// never borrowed across an await and a second read finds `None` and is
/// refused -- see [`Editor::read_line`].
pub struct Editor {
    session: Rc<RefCell<Option<Session<XtermTerminal>>>>,
    names: Arc<Mutex<Vec<String>>>,
    highlight: Rc<Cell<bool>>,
}

/// `drt::repl::Syntax` behind a switch the host flips per line. The
/// native REPL colours every line because every line is code; the page's
/// terminal is a shell before it is a REPL, and a `drt start --config`
/// line coloured as Lua would show `--config` as a comment.
struct Switched {
    on: Rc<Cell<bool>>,
}

impl Highlighter for Switched {
    fn highlight<'l>(&self, line: &'l str) -> Cow<'l, str> {
        if self.on.get() {
            Syntax.highlight(line)
        } else {
            Cow::Borrowed(line)
        }
    }
}

impl Editor {
    /// Take over input and output for the page's xterm.js `Terminal`.
    ///
    /// The object is used duck-typed and never imported, so a bundler, an
    /// import map and a `<script>` tag all work -- the reason `attach` is
    /// shaped this way at all (doc/Browser.md).
    pub fn attach(terminal: JsValue) -> Self {
        let names = Arc::new(Mutex::new(Vec::new()));
        let highlight = Rc::new(Cell::new(true));
        let mut session = Session::new(XtermTerminal::attach(terminal));
        session.set_completer(Names::new(names.clone()));
        session.set_highlighter(Switched {
            on: highlight.clone(),
        });
        Editor {
            session: Rc::new(RefCell::new(Some(session))),
            names,
            highlight,
        }
    }

    /// Replace what Tab completes from.
    ///
    /// The host calls this with `DrtSession::names()` after each accepted
    /// line: a REPL's namespace changes when a line runs, which is exactly
    /// when the snapshot is stale.
    pub fn set_candidates(&self, names: Vec<String>) {
        *self.names.lock().unwrap_or_else(|e| e.into_inner()) = names;
    }

    /// Read one line, showing `prompt`, coloured as Diluvium code when
    /// `highlight` is set.
    ///
    /// The caller chooses the prompt because the page's terminal is a
    /// shell before it is a REPL: `$ ` for a `drt ...` line, then `dv> `
    /// and `>> ` from the session's own `continuing()`, which is what
    /// `repl::edit` does natively with the same two strings. It chooses
    /// the colouring for the same reason: the same `Syntax` natively
    /// paints every `drt repl` line, and only the REPL's lines are code.
    ///
    /// Errors if a read is already in flight -- a host that asks twice has
    /// a bug, and the alternative is a panic across the wasm boundary.
    ///
    /// A future dropped before it finishes takes the session with it and
    /// every later read is refused. That is the honest failure for a
    /// terminal whose editing state went with it, and a host has no reason
    /// to drop one: the promise is awaited by the loop that asked.
    pub fn read_line(
        &self,
        prompt: String,
        highlight: bool,
    ) -> impl Future<Output = Result<Outcome, String>> {
        // Set before the read takes the session: reads are one at a time,
        // so the flag is this line's until the session comes back.
        self.highlight.set(highlight);
        // The future owns a handle rather than borrowing `self`: it becomes
        // a JS promise and outlives this call.
        let held = self.session.clone();
        async move {
            let mut session = held
                .borrow_mut()
                .take()
                .ok_or_else(|| "a line is already being read".to_string())?;
            session.set_prompt(prompt);
            let outcome = session.read_line().await;
            *held.borrow_mut() = Some(session);
            match outcome {
                Ok(ReadOutcome::Line(line)) => Ok(Outcome::Line(line)),
                Ok(ReadOutcome::Interrupted) => Ok(Outcome::Interrupted),
                Ok(ReadOutcome::Eof) => Ok(Outcome::Eof),
                Err(e) => Err(format!("the terminal: {e}")),
            }
        }
    }
}
