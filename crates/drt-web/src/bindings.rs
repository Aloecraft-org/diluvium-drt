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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::editor::{Editor, Outcome};
use crate::ssh;
use crate::swarm::{self, Swarm as SwarmDeployment};
use crate::term::{Session, Step, Term};
use crate::ws;

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

/// The instances table: `dvs.c`'s sixteen, over a `Deployment`.
///
/// `swarm.js` recognises a backend by the shape of its exports, so this is
/// the shape, named the way JavaScript names things and taking ids where
/// `dvs_*` took pointers. Every fallible call throws rather than setting
/// something to poll, which is the one place the table deliberately stops
/// matching -- `dvs_last_error` has no twin here.
#[wasm_bindgen]
pub struct DrtSwarm {
    inner: SwarmDeployment,
}

#[wasm_bindgen]
impl DrtSwarm {
    /// A swarm over this build's connectors, or over the ones `config`
    /// names -- the same JSON `drt run --config` takes.
    ///
    /// Zero for either limit means the swarm's own default, as
    /// `dvsjs_new` meant it.
    #[wasm_bindgen(constructor)]
    pub fn new(
        max_instances: u32,
        spawns_per_step: u32,
        config: Option<String>,
    ) -> Result<DrtSwarm, JsValue> {
        SwarmDeployment::new(max_instances, spawns_per_step, config.as_deref())
            .map(|inner| DrtSwarm { inner })
            .map_err(|e| JsValue::from_str(&e))
    }

    /// The first instance, from source, the capabilities it may hold and
    /// its budget. `caps` and `budget` are a config's own two fields as
    /// JSON, so a page writes what it would write on disk.
    pub fn root(
        &mut self,
        code: &[u8],
        caps: Option<String>,
        budget: Option<String>,
    ) -> Result<u32, JsValue> {
        self.inner
            .root(
                code,
                caps.as_deref().unwrap_or(swarm::DEFAULT_CAPS),
                budget.as_deref().unwrap_or("{}"),
            )
            .map_err(|e| JsValue::from_str(&e))
    }

    /// One round; answers how many instances are alive.
    pub fn step(&mut self) -> usize {
        self.inner.step()
    }

    pub fn alive(&self) -> usize {
        self.inner.alive()
    }

    /// The roster, as ids rather than the pointer `dvs_instance` returned.
    pub fn ids(&self) -> Vec<u32> {
        self.inner.ids()
    }

    #[wasm_bindgen(js_name = slotsAllocated)]
    pub fn slots_allocated(&self) -> usize {
        self.inner.slots_allocated()
    }

    /// Who spawned `id`: 0 for the root, whose parent is nobody, and
    /// `undefined` for an id that is not in the roster. `dvs_parent`
    /// answered 0 for both, having no way to say the second.
    pub fn parent(&self, id: u32) -> Option<u32> {
        self.inner.parent(id)
    }

    pub fn resident(&self, id: u32) -> bool {
        self.inner.resident(id)
    }

    #[wasm_bindgen(js_name = cachedSize)]
    pub fn cached_size(&self, id: u32) -> usize {
        self.inner.cached_size(id)
    }

    #[wasm_bindgen(js_name = wakeOnMessage)]
    pub fn wake_on_message(&self, id: u32) -> bool {
        self.inner.wake_on_message(id)
    }

    /// What `id` may hold, as the JSON a config would have written.
    pub fn caps(&self, id: u32) -> Option<String> {
        self.inner.caps(id)
    }

    pub fn holds(&self, id: u32, cap: &str) -> bool {
        self.inner.holds(id, cap)
    }

    /// Whether `parent` could pass `cap` to something it spawns -- what a
    /// panel asks before offering the button.
    #[wasm_bindgen(js_name = mayGrant)]
    pub fn may_grant(&self, parent: u32, cap: &str) -> bool {
        self.inner.may_grant(parent, cap)
    }

    pub fn budget(&self, id: u32) -> Option<String> {
        self.inner.budget(id)
    }

    /// A msgpack message onto one of `id`'s queues.
    pub fn push(&mut self, id: u32, queue: &str, msg: &[u8]) -> Result<(), JsValue> {
        self.inner
            .push(id, queue, msg)
            .map_err(|e| JsValue::from_str(&e))
    }

    pub fn kill(&mut self, id: u32) -> Result<(), JsValue> {
        self.inner.kill(id).map_err(|e| JsValue::from_str(&e))
    }

    pub fn hibernate(&mut self, id: u32) -> Result<(), JsValue> {
        self.inner.hibernate(id).map_err(|e| JsValue::from_str(&e))
    }

    pub fn wake(&mut self, id: u32) -> Result<(), JsValue> {
        self.inner.wake(id).map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(js_name = allowHibernation)]
    pub fn allow_hibernation(&mut self, allow: bool) {
        self.inner.allow_hibernation(allow);
    }

    #[wasm_bindgen(js_name = allowBytecode)]
    pub fn allow_bytecode(&mut self, allow: bool) {
        self.inner.allow_bytecode(allow);
    }

    #[wasm_bindgen(js_name = allowUnsafeStdlib)]
    pub fn allow_unsafe_stdlib(&mut self, allow: bool) {
        self.inner.allow_unsafe_stdlib(allow);
    }

    #[wasm_bindgen(js_name = setHostIdentity)]
    pub fn set_host_identity(&mut self, identity: Option<String>) {
        self.inner.set_host_identity(identity.as_deref());
    }
}

/// The page's end of a byte stream (`ws.rs`).
///
/// The page owns the socket and pumps this; the Rust half is a `Send`
/// stream a protocol can be handed. What consumes that stream is the
/// caller's -- today [`DrtSocket::start_echo`], to prove the path; an SSH
/// session once `russh` is wired (doc/SshInBrowser.md).
#[wasm_bindgen]
pub struct DrtSocket {
    socket: ws::Socket,
    /// Held apart from `socket` because `nextOutgoing` becomes a promise
    /// that outlives the call: taken for the await and put back after, the
    /// same shape `DrtEditor` uses on its session.
    outgoing: Rc<RefCell<Option<ws::Outgoing>>>,
    /// Set by `close`, and read by the one `nextOutgoing` that can miss it:
    /// a call parked on the await holds the queue, so clearing the cell
    /// does not reach it, and it would put the queue back afterwards and
    /// leave the page pumping into a socket that is gone.
    closed: Rc<Cell<bool>>,
    stream: Option<ws::WsStream>,
}

#[wasm_bindgen]
impl DrtSocket {
    /// A socket and the stream it drives.
    #[wasm_bindgen(constructor)]
    pub fn new() -> DrtSocket {
        let (stream, socket) = ws::channel();
        let mut it = DrtSocket::over(socket);
        it.stream = Some(stream);
        it
    }

    /// The page's half of a stream something else already holds -- what
    /// `DrtSshServer::serve` hands back. `startEcho` has nothing to start
    /// on one of these, and says so.
    pub(crate) fn over(mut socket: ws::Socket) -> DrtSocket {
        let outgoing = socket.take_outgoing();
        DrtSocket {
            socket,
            outgoing: Rc::new(RefCell::new(outgoing)),
            closed: Rc::new(Cell::new(false)),
            stream: None,
        }
    }

    /// Bytes that arrived on the wire. `false` once the stream is gone,
    /// which is the page's cue to stop delivering.
    pub fn deliver(&self, bytes: &[u8]) -> bool {
        self.socket.deliver(bytes.to_vec())
    }

    /// The next chunk to write, or `undefined` when the stream is over --
    /// which is how the page's pump loop learns to close the socket.
    #[wasm_bindgen(js_name = nextOutgoing)]
    pub fn next_outgoing(&self) -> js_sys::Promise {
        let held = self.outgoing.clone();
        let closed = self.closed.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let Some(mut rx) = held.borrow_mut().take() else {
                return Ok(JsValue::UNDEFINED);
            };
            let got = rx.recv().await;
            if !closed.get() {
                *held.borrow_mut() = Some(rx);
            }
            Ok(match got {
                Some(bytes) => js_sys::Uint8Array::from(&bytes[..]).into(),
                None => JsValue::UNDEFINED,
            })
        })
    }

    /// The wire closed: reads see end of input.
    pub fn close(&mut self) {
        self.socket.close();
        self.closed.set(true);
        *self.outgoing.borrow_mut() = None;
    }

    /// Read the stream and write it back upper-cased, until end of input.
    ///
    /// The transport's own gate: it proves bytes cross the page, reach
    /// Rust as a stream, and come back, without a protocol in the way. The
    /// SSH session replaces this consumer and nothing else.
    ///
    /// It ships. The browser gate builds the profile the release builds,
    /// so a diagnostic held out of the artifact is a diagnostic nothing
    /// tests -- and a host wiring a socket up wants to check the plumbing
    /// before a protocol is in the way.
    #[wasm_bindgen(js_name = startEcho)]
    pub fn start_echo(&mut self) -> Result<(), JsValue> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| JsValue::from_str("this socket's stream is already taken"))?;
        wasm_bindgen_futures::spawn_local(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf[..n].make_ascii_uppercase();
                        if stream.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = stream.shutdown().await;
        });
        Ok(())
    }
}

impl Default for DrtSocket {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// depth: SSH into the page (doc/SshInBrowser.md)
// ---------------------------------------------------------------------------

/// An SSH server the page serves connections to.
///
/// The posture is `ssh.rs`'s and it is not softened here: a host key the
/// page keeps, `authorized_keys` lines naming who may log in, and no way
/// to say "anyone". `generateHostKey` hands a key *back* rather than
/// holding one, because a host key that changes on reload trains whoever
/// connects to click through the warning that says it changed.
///
/// ```js
/// const server = new DrtSshServer(hostKey, authorizedKeys);
/// const socket = server.serve((shell) => attachShell(shell));
/// // ...then pump `socket` against a WebSocket, as with DrtSocket.
/// ```
#[wasm_bindgen]
pub struct DrtSshServer {
    /// Kept as text and parsed per connection: `Config` wants an owned
    /// key, and one Ed25519 parse per connection is nothing next to a key
    /// exchange. Validated in the constructor, so a bad key is an error
    /// where it was typed rather than when someone connects.
    host_key: String,
    fingerprint: String,
    authorized: ssh::Authorized,
}

#[wasm_bindgen]
impl DrtSshServer {
    /// `hostKey` is an OpenSSH private key; `authorizedKeys` is the
    /// contents of an `authorized_keys` file. An empty one authenticates
    /// nobody, which is what a page that has not decided should get.
    #[wasm_bindgen(constructor)]
    pub fn new(host_key: &str, authorized_keys: &str) -> Result<DrtSshServer, JsValue> {
        let parsed =
            ssh::HostKey::parse(host_key).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let authorized = ssh::Authorized::parse(authorized_keys)
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        Ok(DrtSshServer {
            host_key: host_key.to_string(),
            fingerprint: parsed.fingerprint(),
            authorized,
        })
    }

    /// A fresh OpenSSH private key, for the page to store. Not held here:
    /// see the type's note.
    #[wasm_bindgen(js_name = generateHostKey)]
    pub fn generate_host_key() -> Result<String, JsValue> {
        ssh::HostKey::generate().map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// `SHA256:...`, what `ssh` prints on first connection. Show it beside
    /// the terminal and whoever connects can check it instead of trusting
    /// on first use.
    #[wasm_bindgen(getter)]
    pub fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }

    /// How many keys may log in. `0` is a server nobody can reach.
    #[wasm_bindgen(getter)]
    pub fn authorized(&self) -> usize {
        self.authorized.len()
    }

    /// Serve one connection, and return the socket the page pumps for it
    /// -- the same `DrtSocket` contract, because it is the same transport.
    ///
    /// `onShell` is called with a [`DrtShell`] each time the client asks
    /// for a terminal.
    pub fn serve(&self, on_shell: js_sys::Function) -> Result<DrtSocket, JsValue> {
        let host_key =
            ssh::HostKey::parse(&self.host_key).map_err(|e| JsValue::from_str(&e.to_string()))?;
        let authorized = self.authorized.clone();
        let (stream, socket) = ws::channel();
        let (shells, mut opened) = tokio::sync::mpsc::channel(4);

        // Two tasks rather than one: the connection's future is `Send` and
        // holds no JS (that is what lets russh have it), and the loop that
        // calls into the page is not `Send` and never enters russh. The
        // channel between them is the seam.
        wasm_bindgen_futures::spawn_local(async move {
            let _ = ssh::serve(stream, host_key, authorized, shells).await;
        });
        wasm_bindgen_futures::spawn_local(async move {
            while let Some(shell) = opened.recv().await {
                let window = shell.window.clone();
                let (reader, writer) = shell.split();
                // One task drains the writes, so what a terminal writes
                // reaches the client in the order it was written. Without
                // it every `write` would be its own promise racing the
                // others, and a redrawn line would arrive scrambled.
                let (to_client, mut queued) = tokio::sync::mpsc::channel::<ToClient>(ws::DEPTH);
                wasm_bindgen_futures::spawn_local(async move {
                    while let Some(next) = queued.recv().await {
                        match next {
                            ToClient::Data(bytes) => {
                                if writer.write(bytes).await.is_err() {
                                    break;
                                }
                            }
                            // In the queue rather than beside it: a close
                            // that overtook a pending write would cut off
                            // the last thing the program said.
                            ToClient::Close(status) => {
                                writer.close(status).await;
                                break;
                            }
                        }
                    }
                });
                let handed = DrtShell {
                    window,
                    reader: Rc::new(RefCell::new(Some(reader))),
                    to_client,
                };
                if on_shell
                    .call1(&JsValue::NULL, &JsValue::from(handed))
                    .is_err()
                {
                    // The page threw. Its terminal is not coming, and the
                    // client is better told than left at a blank screen.
                    break;
                }
            }
        });
        Ok(DrtSocket::over(socket))
    }
}

/// One SSH session's terminal.
///
/// Shaped for the object `attach` already takes (doc/Browser.md): bytes
/// out, bytes in, and a window. What turns it into that object is a few
/// lines of JS -- `ssh-terminal.js` -- rather than a second terminal
/// implementation in Rust.
#[wasm_bindgen]
pub struct DrtShell {
    /// Read on every `cols`/`rows`, because a client can resize mid
    /// session and `ego_cli` asks a terminal its size on every keystroke.
    window: ssh::Window,
    /// Taken for the read's await and put back after, as `DrtEditor` does
    /// with its session: a promise outlives the call that made it.
    reader: Rc<RefCell<Option<ssh::ShellReader>>>,
    /// Writes go through one queue, drained in order by the task
    /// `DrtSshServer::serve` spawned. See there for why.
    to_client: tokio::sync::mpsc::Sender<ToClient>,
}

/// What is queued towards the client, in order.
enum ToClient {
    Data(Vec<u8>),
    Close(u32),
}

#[wasm_bindgen]
impl DrtShell {
    /// The client's window, as of now -- a getter rather than a number,
    /// so a host reading it on every keystroke sees a resize the same way
    /// it would from xterm.js.
    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u32 {
        self.window.get().0
    }

    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u32 {
        self.window.get().1
    }

    /// What the client typed, or `undefined` once the session is over.
    pub fn read(&self) -> js_sys::Promise {
        let held = self.reader.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let Some(mut reader) = held.borrow_mut().take() else {
                return Ok(JsValue::UNDEFINED);
            };
            let got = reader.read().await;
            *held.borrow_mut() = Some(reader);
            Ok(match got {
                Some(bytes) => js_sys::Uint8Array::from(&bytes[..]).into(),
                None => JsValue::UNDEFINED,
            })
        })
    }

    /// Write to the client's terminal, queued behind whatever is already
    /// waiting. `false` once the session is gone.
    ///
    /// Not a promise, and that is the point: xterm.js's `write` is
    /// fire-and-forget, so a host can hand this object straight to
    /// anything that drives a terminal, and ordering is the queue's
    /// business rather than the caller's.
    pub fn write(&self, bytes: Vec<u8>) -> bool {
        self.to_client.try_send(ToClient::Data(bytes)).is_ok()
    }

    /// End the session, the way a shell exiting does. Queued behind the
    /// writes, so the last thing the program said still arrives.
    pub fn close(&self, status: u32) -> bool {
        self.to_client.try_send(ToClient::Close(status)).is_ok()
    }
}
