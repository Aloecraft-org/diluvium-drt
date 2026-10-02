//! The REPL as bytes, in the page (`doc/P2P.md` §5.2, `doc/Browser.md`).
//!
//! What `drt p2p`'s named service `repl` is natively: a REPL whose
//! terminal is a byte stream with the size reported beside it, line
//! editing inside, so whoever holds the stream sends keystrokes and paints
//! what comes back and nothing more. This is the same shape for the
//! in-page root, so a launcher's "attach a terminal to a root" is one
//! thing whether the root is this module or a `drt` reached over WebRTC.
//!
//! The editor is the same `ego_cli::Session` as `editor.rs` and the native
//! `sshd.rs`, over a terminal whose keys arrive as bytes decoded by the
//! same `AnsiDecoder` the native service uses, and whose writes go to a
//! sink the page gave. The §5 loop runs here, in a spawned task, rather
//! than in the page: the page holds no `read_line`, only a stream.
//!
//! ## surface block
//!
//! - [`Raw::start`]: a session over `Term::exec(["drt", "repl"])`, its bytes
//!   to `sink`, its end to `closed`. One at a time in a page: the runtime's
//!   `print` reaches one sink.
//! - [`Raw::input`], [`Raw::resize`], [`Raw::close`]: what the stream's
//!   holder does.
//! - [`PROMPT`], [`CONTINUING`]: the two prompts, as the native REPL's.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Waker;

use drt::repl::Names;
use ego_cli::term::{Capabilities, Event, Size, Terminal};
use ego_cli::{ReadOutcome, Session as Editor};
use wasm_bindgen::prelude::*;

use crate::term::{Session, Step, Term};

pub const PROMPT: &str = "dv> ";
pub const CONTINUING: &str = ">> ";

thread_local! {
    /// Whether a raw session holds the runtime's output.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// What the terminal and the stream's holder share.
struct Shared {
    /// Keystrokes not yet decoded.
    queue: VecDeque<u8>,
    /// Who waits for them.
    waker: Option<Waker>,
    size: Size,
    closed: bool,
}

impl Shared {
    fn wake(&mut self) {
        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }
}

/// A raw REPL session's handle: the stream's holder's side.
pub struct Raw {
    shared: Rc<RefCell<Shared>>,
}

impl Raw {
    /// Start a REPL whose terminal is the stream: keystrokes in through
    /// [`Raw::input`], everything it writes out through `sink(bytes)`,
    /// `closed(status)` once, when it ends. Refused while another runs.
    pub fn start(
        term: &Term,
        cols: u16,
        rows: u16,
        sink: js_sys::Function,
        closed: js_sys::Function,
    ) -> Result<Raw, String> {
        if ACTIVE.with(|a| a.get()) {
            return Err("one raw REPL session at a time in a page".into());
        }
        ACTIVE.with(|a| a.set(true));
        let shared = Rc::new(RefCell::new(Shared {
            queue: VecDeque::new(),
            waker: None,
            size: Size::new(cols.max(1), rows.max(1)),
            closed: false,
        }));
        let session = term.exec(&["drt".to_string(), "repl".to_string()]);
        // While this runs, the runtime's print is the stream's; the page's
        // own sink comes back when it ends.
        let out = sink.clone();
        let previous = drt_platform::stdio::install_sink(Box::new(move |_fd, bytes| {
            let _ = out.call1(&JsValue::NULL, &js_sys::Uint8Array::from(&crlf(bytes)[..]));
        }));
        let terminal = ByteTerminal {
            shared: shared.clone(),
            sink,
            decoder: ego_cli::decode::AnsiDecoder::new(),
            pending: VecDeque::new(),
            partial: Vec::new(),
            last: Size::new(cols.max(1), rows.max(1)),
        };
        let handle = shared.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let status = drive(session, terminal).await;
            handle.borrow_mut().closed = true;
            match previous {
                Some(p) => {
                    drt_platform::stdio::install_sink(p);
                }
                None => {
                    drt_platform::stdio::uninstall_sink();
                }
            }
            ACTIVE.with(|a| a.set(false));
            let _ = closed.call1(&JsValue::NULL, &JsValue::from(status));
        });
        Ok(Raw { shared })
    }

    /// Keystrokes from the far side, as the terminal sent them.
    pub fn input(&self, bytes: &[u8]) {
        let mut s = self.shared.borrow_mut();
        if s.closed {
            return;
        }
        s.queue.extend(bytes);
        s.wake();
    }

    /// The far side's terminal changed size.
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut s = self.shared.borrow_mut();
        s.size = Size::new(cols.max(1), rows.max(1));
        s.wake();
    }

    /// The far side went away: the REPL reads end of input and ends.
    pub fn close(&self) {
        let mut s = self.shared.borrow_mut();
        s.closed = true;
        s.wake();
    }
}

// depth: the §5 loop, in a task

/// `Step::Input` is where a line is read; a sleep is a timer; exit is the
/// status. Ends when the session ends or the stream closes.
async fn drive(mut session: Session, terminal: ByteTerminal) -> i32 {
    let names = Arc::new(Mutex::new(Vec::new()));
    let mut editor = Editor::new(terminal);
    editor.set_completer(Names::new(names.clone()));
    loop {
        match session.tick() {
            Step::Sleep(d) => sleep_ms(d.as_secs_f64() * 1000.0).await,
            Step::Input { continuing } => {
                *names.lock().unwrap_or_else(|e| e.into_inner()) = session.names();
                editor.set_prompt(if continuing { CONTINUING } else { PROMPT });
                match editor.read_line().await {
                    Ok(ReadOutcome::Line(line)) => {
                        let _ = session.feed(&line);
                    }
                    Ok(ReadOutcome::Interrupted) => session.abandon(),
                    // End of input, newline included, as the native REPL leaves.
                    Ok(ReadOutcome::Eof) | Err(_) => {
                        let _ = editor.terminal_mut().write("\r\n").await;
                        return 0;
                    }
                }
            }
            Step::Exit(status) => return status,
        }
    }
}

async fn sleep_ms(ms: f64) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let global = js_sys::global();
        let set_timeout = js_sys::Reflect::get(&global, &JsValue::from_str("setTimeout"))
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
        match set_timeout {
            Some(f) => {
                let _ = f.call2(&global, &resolve, &JsValue::from_f64(ms));
            }
            None => {
                let _ = resolve.call0(&JsValue::NULL);
            }
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The runtime's bytes with `\n` made `\r\n`, as a terminal wants and as
/// the native service does.
fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut prev = 0u8;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

// depth: the terminal the editor edits on

/// The stream as the terminal `ego_cli` edits a line on: bytes decoded to
/// keys, the reported size as the size, writes to the sink.
struct ByteTerminal {
    shared: Rc<RefCell<Shared>>,
    sink: js_sys::Function,
    decoder: ego_cli::decode::AnsiDecoder,
    pending: VecDeque<ego_cli::KeyPress>,
    /// The tail of a UTF-8 character split across two inputs.
    partial: Vec<u8>,
    /// The size the editor was last told, so a change is reported once.
    last: Size,
}

impl ByteTerminal {
    /// Wait for input, a resize, or the close.
    async fn wait(&self) -> Wait {
        std::future::poll_fn(|cx| {
            let mut s = self.shared.borrow_mut();
            if s.closed {
                return std::task::Poll::Ready(Wait::Closed);
            }
            if !s.queue.is_empty() {
                let bytes: Vec<u8> = s.queue.drain(..).collect();
                return std::task::Poll::Ready(Wait::Bytes(bytes));
            }
            s.waker = Some(cx.waker().clone());
            std::task::Poll::Pending
        })
        .await
    }
}

enum Wait {
    Bytes(Vec<u8>),
    Closed,
}

impl Terminal for ByteTerminal {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            raw_mode: true,
            ansi: true,
            resize_events: true,
            line_discipline: false,
        }
    }

    fn size(&self) -> Size {
        self.shared.borrow().size
    }

    fn set_raw(&mut self, _enabled: bool) -> ego_cli::Result<()> {
        // The far side's terminal is raw already: that is what a byte
        // stream of keystrokes means.
        Ok(())
    }

    async fn next_event(&mut self) -> ego_cli::Result<Event> {
        loop {
            let now = self.shared.borrow().size;
            if now != self.last {
                self.last = now;
                return Ok(Event::Resize(now));
            }
            if let Some(key) = self.pending.pop_front() {
                return Ok(Event::Key(key));
            }
            let bytes = match self.wait().await {
                Wait::Bytes(b) => b,
                Wait::Closed => return Ok(Event::Eof),
            };
            self.partial.extend_from_slice(&bytes);
            let valid = match std::str::from_utf8(&self.partial) {
                Ok(_) => self.partial.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => {
                    self.partial.clear();
                    continue;
                }
            };
            let text: Vec<u8> = self.partial.drain(..valid).collect();
            let text = String::from_utf8(text).unwrap_or_default();
            self.pending.extend(self.decoder.push(&text));
        }
    }

    async fn write(&mut self, text: &str) -> ego_cli::Result<()> {
        let _ = self
            .sink
            .call1(&JsValue::NULL, &js_sys::Uint8Array::from(text.as_bytes()));
        Ok(())
    }

    async fn flush(&mut self) -> ego_cli::Result<()> {
        Ok(())
    }
}
