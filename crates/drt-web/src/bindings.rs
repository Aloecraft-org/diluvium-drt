//! The wasm-bindgen surface: `term`'s contract as JS classes, and nothing
//! decided here.
//!
//! **Exports must not panic.** On `wasm32-unknown-unknown` a Rust panic is
//! a trap, not an unwind: `catch_unwind` never runs, `RuntimeError:
//! unreachable` is thrown into JS, and the module keeps answering with
//! whatever invariants the panic left broken -- established in a browser
//! by the earlier export layer (doc/Wasm.md §2.4). So the bodies below
//! return errors instead, and a page that catches a trap discards the
//! module. `setPanicHook` makes the reason visible on the console first.

use wasm_bindgen::prelude::*;

use crate::editor::{Editor, Outcome};
use crate::term::{Session, Step, Term};

/// The flag wasm-bindgen 0.2.114 reads before every export call once a
/// module carries exception-handling instructions -- which this one does,
/// for the C core's `longjmp` (doc/Wasm.md §2.3). The crate defines it
/// only under `panic = "unwind"`, and a browser build panics by aborting,
/// so it is defined here: a `u32` the linker exports as an `i32` global
/// holding its address, which is the shape the glue expects. It is zero
/// until the glue marks the instance terminated. Delete when the pin in
/// `Cargo.toml` moves to >= 0.2.127, which defines it unconditionally.
#[no_mangle]
pub static mut __instance_terminated: u32 = 0;

extern "C" {
    /// wasi-libc's constructors, as the linker collected them.
    fn __wasm_call_ctors();
}

/// Run once by the glue's `init()`, before any other export: the reactor
/// convention, done by hand. It matters for what it prevents as much as
/// for what it runs. A module that carries constructors and never calls
/// this gets the *command* treatment from wasm-ld instead -- every export
/// wrapped to run the constructors before and libc's destructors after --
/// and libc's destructor flushes stdout, which writes to the sink, which
/// allocates a JS value through an export, which runs the destructors,
/// which flush stdout. Measured as a stack overflow on the first `print`
/// (doc/Wasm.md §2.3).
#[wasm_bindgen(start)]
pub fn start() {
    // SAFETY: a linker-synthesised function with no arguments and no
    // result, called exactly once, at load, before anything touches libc.
    unsafe { __wasm_call_ctors() };
}

/// Print a panic's message to the console before it traps.
#[wasm_bindgen(js_name = setPanicHook)]
pub fn set_panic_hook() {
    console_error_panic_hook::set_once();
}

/// The dv ABI the linked C core speaks. The wasm equivalent of `drt
/// --version`: the one call a smoke test makes to prove the module is
/// alive.
#[wasm_bindgen(js_name = abiVersion)]
pub fn abi_version() -> u32 {
    drt_swarm::engine::abi_versions()
        .map(|(library, _)| library)
        .unwrap_or(0)
}

/// `drt buildinfo`, from inside the page: what this artifact carries, read
/// off the artifact rather than guessed from its filename.
#[wasm_bindgen(js_name = buildInfo)]
pub fn build_info(json: bool) -> String {
    drt::cli::buildinfo(json)
}

/// The terminal: a filesystem to seed, and `exec`.
#[wasm_bindgen]
pub struct DrtTerm {
    inner: Term,
}

#[wasm_bindgen]
impl DrtTerm {
    /// `sink(fd, bytes)` receives every byte the runtime writes -- the C
    /// core's `print`, the REPL's answers, a `drt run:` refusal -- with
    /// `fd` 1 or 2 and `bytes` a `Uint8Array`, in order.
    #[wasm_bindgen(constructor)]
    pub fn new(sink: js_sys::Function) -> DrtTerm {
        drt_platform::stdio::install_sink(Box::new(move |fd, bytes| {
            let fd = match fd {
                drt_platform::stdio::Fd::Stdout => 1u32,
                drt_platform::stdio::Fd::Stderr => 2u32,
            };
            let _ = sink.call2(
                &JsValue::NULL,
                &JsValue::from(fd),
                &js_sys::Uint8Array::from(bytes),
            );
        }));
        DrtTerm { inner: Term::new() }
    }

    /// Put a file in the terminal's filesystem, making the directories
    /// above it. Paths are absolute, or relative to the working directory
    /// (`/` until `setCwd`).
    #[wasm_bindgen(js_name = putFile)]
    pub fn put_file(&self, path: &str, bytes: &[u8]) {
        self.inner.fs().add_file(path, bytes);
    }

    /// Make a directory, empty. A granted scope that holds no files yet
    /// still has to exist.
    #[wasm_bindgen(js_name = putDir)]
    pub fn put_dir(&self, path: &str) {
        self.inner.fs().add_dir(path);
    }

    /// Read a file back, or `undefined`.
    #[wasm_bindgen(js_name = getFile)]
    pub fn get_file(&self, path: &str) -> Option<Vec<u8>> {
        use drt_platform::fs::Backend;
        self.inner.fs().read(std::path::Path::new(path)).ok()
    }

    /// Every file, by absolute path.
    #[wasm_bindgen(js_name = listFiles)]
    pub fn list_files(&self) -> Vec<String> {
        self.inner
            .fs()
            .files()
            .into_iter()
            .map(|(p, _)| p.display().to_string())
            .collect()
    }

    /// The directory relative paths resolve against -- what `cd` is.
    #[wasm_bindgen(js_name = setCwd)]
    pub fn set_cwd(&self, path: &str) {
        self.inner.fs().set_cwd(path);
    }

    #[wasm_bindgen(js_name = cwd)]
    pub fn cwd(&self) -> String {
        self.inner.fs().cwd().display().to_string()
    }

    /// Run one command line: `["drt", "run", "app.dlua"]`.
    pub fn exec(&self, argv: Vec<String>) -> DrtSession {
        DrtSession {
            inner: self.inner.exec(&argv),
        }
    }
}

/// One command, ticked to completion by the page.
#[wasm_bindgen]
pub struct DrtSession {
    inner: Session,
}

#[wasm_bindgen]
impl DrtSession {
    /// Advance. Answers `{sleepMs}`, `{wantsInput: true, continuing}`, or
    /// `{done: true, status}`.
    pub fn tick(&mut self) -> JsValue {
        let o = js_sys::Object::new();
        let set = |k: &str, v: JsValue| {
            let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), &v);
        };
        match self.inner.tick() {
            Step::Sleep(d) => set("sleepMs", JsValue::from_f64(d.as_secs_f64() * 1000.0)),
            Step::Input { continuing } => {
                set("wantsInput", JsValue::TRUE);
                set("continuing", JsValue::from_bool(continuing));
            }
            Step::Exit(status) => {
                set("done", JsValue::TRUE);
                set("status", JsValue::from(status));
            }
        }
        o.into()
    }

    /// Feed the REPL one line. `true` when it was sent; a blank line
    /// outside a continuation is not.
    pub fn feed(&mut self, line: &str) -> Result<bool, JsValue> {
        self.inner.feed(line).map_err(|e| JsValue::from_str(&e))
    }

    pub fn continuing(&self) -> bool {
        self.inner.continuing()
    }

    #[wasm_bindgen(js_name = isOver)]
    pub fn is_over(&self) -> bool {
        self.inner.is_over()
    }

    /// The names Tab completes from, as of the last accepted line.
    pub fn names(&self) -> Vec<String> {
        self.inner.names()
    }

    /// Throw away an unfinished line, as Ctrl+C does natively.
    pub fn abandon(&mut self) {
        self.inner.abandon();
    }
}

/// The line editor over the page's own xterm.js `Terminal`.
///
/// One `read_line` at a time, because §5 puts exactly one in a host: the
/// tick loop stays in the page and calls this only where the driver parks
/// on input. What it gets in return is what a tty gets -- history, word
/// motions, undo, and Tab over the guest's names -- from the same
/// `ego_cli::Session` and the same `drt::repl::Names`.
#[wasm_bindgen]
pub struct DrtEditor {
    inner: Editor,
}

#[wasm_bindgen]
impl DrtEditor {
    /// Take the page's terminal object. Nothing is imported: `attach` uses
    /// it duck-typed, so a bundler, an import map and a `<script>` tag all
    /// work.
    pub fn attach(terminal: JsValue) -> DrtEditor {
        DrtEditor {
            inner: Editor::attach(terminal),
        }
    }

    /// Read one line, showing `prompt`.
    ///
    /// Resolves to `{line}`, `{interrupted: true}` (Ctrl+C) or
    /// `{eof: true}` (Ctrl+D on an empty line); rejects if a read is
    /// already in flight.
    #[wasm_bindgen(js_name = readLine)]
    pub fn read_line(&self, prompt: String) -> js_sys::Promise {
        let reading = self.inner.read_line(prompt);
        wasm_bindgen_futures::future_to_promise(async move {
            let o = js_sys::Object::new();
            let set = |k: &str, v: JsValue| {
                let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), &v);
            };
            match reading.await {
                Ok(Outcome::Line(line)) => set("line", JsValue::from_str(&line)),
                Ok(Outcome::Interrupted) => set("interrupted", JsValue::TRUE),
                Ok(Outcome::Eof) => set("eof", JsValue::TRUE),
                Err(e) => return Err(JsValue::from_str(&e)),
            }
            Ok(o.into())
        })
    }

    /// Replace what Tab serves from -- `DrtSession.names()`, after each
    /// accepted line.
    #[wasm_bindgen(js_name = setCandidates)]
    pub fn set_candidates(&self, names: Vec<String>) {
        self.inner.set_candidates(names);
    }
}
