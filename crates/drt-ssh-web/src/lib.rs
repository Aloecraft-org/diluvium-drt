//! SSH in a browser (`doc/Plan-0.8.0.md` §2): russh compiled to wasm,
//! carried over a WebSocket to the relay's claim door, `/s/<label>`.
//!
//! The page sends exactly the bytes `ssh -o ProxyCommand="drt tunnel …"`
//! sends, so the relay and the parked device are unchanged, and the SSH
//! session -- key exchange, host key, authentication -- is end to end
//! between this page and sshd. The relay only ever carries ciphertext.
//!
//! **The host key is decided between key exchange and authentication.**
//! `Ssh.connect` resolves once the server's key is known, and nothing that
//! authenticates has been sent yet: the page compares `hostKey` with the
//! fingerprint the link pinned or with what it trusted on first use, and
//! only then calls `authPassword` or `authKey`. A pinned fingerprint that
//! does not match fails the connect itself. This is also why russh's
//! handler, which must be `Send`, never has to call into JavaScript.
//!
//! ## surface block
//!
//! - Entry points (JS): `Ssh.connect(url, pinned?)`; then `hostKey`,
//!   `authPassword(user, password)`, `authKey(user, openssh)`,
//!   `shell(cols, rows, onData, onClose)`, `write(bytes)`,
//!   `resize(cols, rows)`, `keepalive()`, `close()`; and
//!   `generateKey(comment)`.
//! - Configurable: [`PIPE`], [`TERM`].
//!
//! **No timer runs in here.** russh's keepalive and inactivity timers are
//! `tokio::time`, which reads `std::time::Instant` -- and that panics on
//! `wasm32-unknown-unknown`, which has no clock std can reach. Both stay at
//! their default, off, and the page calls `keepalive()` on a browser timer
//! instead, so a quiet session still never meets the relay's idle close.
//!
//! - Fan-out: [`Op`], what the page asks of an open shell; a failed connect
//!   carries `code`: the WebSocket close code (1013 is "the device is not
//!   home", which the page retries) or `"hostkey"` for a pin mismatch.

#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use js_sys::{Function, Promise, Reflect, Uint8Array};
use russh::client::{self, Handle};
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::ssh_key::{HashAlg, LineEnding};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, oneshot};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{future_to_promise, spawn_local};
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

/// Runs once, when the module is instantiated: a Rust panic is a wasm trap
/// on this target, and this puts its message on the console before it.
#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

/// The in-memory pipe between the WebSocket and russh, each way.
pub const PIPE: usize = 256 * 1024;
/// What the pty says it is. xterm.js is the terminal on the other side.
pub const TERM: &str = "xterm-256color";

/// What the page asks of an open shell, in the order it asked.
enum Op {
    Data(Vec<u8>),
    Resize(u32, u32),
    Eof,
}

/// Records the server's key and accepts it unless a pin says otherwise.
/// Certificates are out of scope and refused.
struct Checker {
    pinned: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        *self.seen.lock().expect("one thread") = Some(fingerprint.clone());
        Ok(self.pinned.as_ref().is_none_or(|p| *p == fingerprint))
    }
}

/// A JS `Error` carrying a machine-readable `code` beside its message.
fn fail(message: &str, code: Option<JsValue>) -> JsValue {
    let e = js_sys::Error::new(message);
    if let Some(code) = code {
        let _ = Reflect::set(&e, &"code".into(), &code);
    }
    e.into()
}

fn rejected(e: impl std::fmt::Display) -> JsValue {
    fail(&e.to_string(), None)
}

// depth: the WebSocket, spliced to a pipe russh can own

/// The WebSocket and the Rust closures its handlers call, which must go
/// together: dropping this detaches the handlers before they are freed, so
/// the browser never calls into a dropped closure, and closes the socket.
/// Every way out of a connect, and the end of a session, goes through here.
struct Socket {
    ws: WebSocket,
    _handlers: Vec<Box<dyn std::any::Any>>,
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
    }
}

/// Open `url` and resolve once it is open, with russh's end of a pipe whose
/// other end two tasks splice to the socket, bytes in order both ways.
/// `closed` records the socket's close code whenever it arrives.
async fn open(
    url: &str,
    closed: Rc<RefCell<Option<u16>>>,
) -> Result<(Socket, DuplexStream), JsValue> {
    let ws = WebSocket::new(url).map_err(|e| fail(&format!("not a WebSocket URL: {e:?}"), None))?;
    ws.set_binary_type(BinaryType::Arraybuffer);
    let (ours, theirs) = tokio::io::duplex(PIPE);
    let (inbound, mut inbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let inbound = Rc::new(RefCell::new(Some(inbound)));
    let (opened, opened_rx) = oneshot::channel::<Result<(), u16>>();
    let opened = Rc::new(RefCell::new(Some(opened)));

    let on_open = {
        let opened = opened.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(tx) = opened.borrow_mut().take() {
                let _ = tx.send(Ok(()));
            }
        })
    };
    let on_message = {
        let inbound = inbound.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let (Some(tx), Ok(buf)) = (
                inbound.borrow().as_ref(),
                e.data().dyn_into::<js_sys::ArrayBuffer>(),
            ) {
                let _ = tx.send(Uint8Array::new(&buf).to_vec());
            }
        })
    };
    let on_close = {
        let (opened, inbound, closed) = (opened.clone(), inbound.clone(), closed.clone());
        Closure::<dyn FnMut(CloseEvent)>::new(move |e: CloseEvent| {
            *closed.borrow_mut() = Some(e.code());
            // Before open, the connect fails with the code; after, dropping
            // the sender is russh's end of file.
            if let Some(tx) = opened.borrow_mut().take() {
                let _ = tx.send(Err(e.code()));
            }
            inbound.borrow_mut().take();
        })
    };
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    let socket = Socket {
        ws: ws.clone(),
        _handlers: vec![Box::new(on_open), Box::new(on_message), Box::new(on_close)],
    };

    match opened_rx.await {
        Ok(Ok(())) => {}
        // 1006 before open is everything a browser will not explain: a 403
        // for a bad key or label, a relay that is down, a TLS failure.
        Ok(Err(code)) => {
            let why = if code == 1006 {
                "the relay refused the connection or could not be reached (a bad key or label, or the relay is down)"
            } else {
                "the relay closed the connection before it opened"
            };
            return Err(fail(why, Some(JsValue::from(code))));
        }
        Err(_) => return Err(fail("the WebSocket went away before it opened", None)),
    }

    let (mut from_russh, mut to_russh) = tokio::io::split(theirs);
    spawn_local(async move {
        while let Some(bytes) = inbound_rx.recv().await {
            if to_russh.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = to_russh.shutdown().await;
    });
    let sock = ws.clone();
    spawn_local(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match from_russh.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = sock.close();
                    break;
                }
                Ok(n) => {
                    if sock.send_with_u8_array(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok((socket, ours))
}

// depth: the session

struct Inner {
    handle: Option<Handle<Checker>>,
    host_key: String,
    socket: Socket,
    ops: Option<mpsc::UnboundedSender<Op>>,
}

fn take_handle(inner: &Rc<RefCell<Inner>>) -> Result<Handle<Checker>, JsValue> {
    inner
        .borrow_mut()
        .handle
        .take()
        .ok_or_else(|| fail("the session is busy or closed", None))
}

/// One SSH session to one host, over one WebSocket.
#[wasm_bindgen]
pub struct Ssh {
    inner: Rc<RefCell<Inner>>,
}

#[wasm_bindgen]
impl Ssh {
    /// Open the WebSocket and run key exchange. Resolves once the server's
    /// host key is known and before anything authenticates; rejects with
    /// `code` set to the close code, or to `"hostkey"` when `pinned` (a
    /// `SHA256:…` fingerprint) does not match.
    pub async fn connect(url: String, pinned: Option<String>) -> Result<Ssh, JsValue> {
        let closed = Rc::new(RefCell::new(None));
        let (socket, stream) = open(&url, closed.clone()).await?;
        let seen = Arc::new(Mutex::new(None));
        // Timers off, deliberately: see the module note.
        let config = client::Config::default();
        let checker = Checker {
            pinned: pinned.clone(),
            seen: seen.clone(),
        };
        match client::connect_stream(Arc::new(config), stream, checker).await {
            Ok(handle) => {
                let host_key = seen.lock().expect("one thread").clone().unwrap_or_default();
                Ok(Ssh {
                    inner: Rc::new(RefCell::new(Inner {
                        handle: Some(handle),
                        host_key,
                        socket,
                        ops: None,
                    })),
                })
            }
            Err(e) => {
                drop(socket);
                let saw = seen.lock().expect("one thread").clone();
                if let (Some(pin), Some(saw)) = (&pinned, &saw) {
                    if pin != saw {
                        return Err(fail(
                            &format!("the host key is {saw}, and the link pinned {pin}: refusing to go on"),
                            Some("hostkey".into()),
                        ));
                    }
                }
                let code = *closed.borrow();
                match code {
                    Some(1013) => Err(fail(
                        "the device is not home: nothing is parked at that label",
                        Some(1013.into()),
                    )),
                    Some(1008) => Err(fail(
                        "the relay's deployment refused this connection",
                        Some(1008.into()),
                    )),
                    Some(c) => Err(fail(
                        &format!("the connection closed ({c}) during key exchange: {e}"),
                        Some(c.into()),
                    )),
                    None => Err(fail(&format!("key exchange failed: {e}"), None)),
                }
            }
        }
    }

    /// The server's host key, `SHA256:…`, as `ssh-keygen -lf` prints it.
    #[wasm_bindgen(getter, js_name = hostKey)]
    pub fn host_key(&self) -> String {
        self.inner.borrow().host_key.clone()
    }

    /// Password authentication. Resolves `true` when accepted.
    #[wasm_bindgen(js_name = authPassword)]
    pub fn auth_password(&self, user: String, password: String) -> Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let mut handle = take_handle(&inner)?;
            let result = handle.authenticate_password(user, password).await;
            inner.borrow_mut().handle = Some(handle);
            Ok(result.map_err(rejected)?.success().into())
        })
    }

    /// Public-key authentication with an OpenSSH private key (as
    /// `generateKey` makes, or an unencrypted one pasted in). Resolves
    /// `true` when accepted.
    #[wasm_bindgen(js_name = authKey)]
    pub fn auth_key(&self, user: String, openssh: String) -> Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let key = PrivateKey::from_openssh(openssh.as_bytes()).map_err(rejected)?;
            let mut handle = take_handle(&inner)?;
            let result = async {
                // An RSA key signs with the best hash the server offers;
                // for any other kind the hash is not a choice.
                let hash = if key.algorithm().is_rsa() {
                    handle.best_supported_rsa_hash().await?.flatten()
                } else {
                    None
                };
                handle
                    .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
            }
            .await;
            inner.borrow_mut().handle = Some(handle);
            Ok(result.map_err(rejected)?.success().into())
        })
    }

    /// Open a pty and a shell. `onData(Uint8Array)` gets everything the
    /// shell writes, stdout and stderr alike, as a terminal would;
    /// `onClose(exitStatus | null)` is called once when it ends.
    pub fn shell(&self, cols: u32, rows: u32, on_data: Function, on_close: Function) -> Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let handle = take_handle(&inner)?;
            let opened = async {
                let channel = handle.channel_open_session().await?;
                channel
                    .request_pty(false, TERM, cols, rows, 0, 0, &[])
                    .await?;
                channel.request_shell(false).await?;
                Ok::<_, russh::Error>(channel)
            }
            .await;
            inner.borrow_mut().handle = Some(handle);
            let (mut read, write) = opened.map_err(rejected)?.split();

            let (ops, mut ops_rx) = mpsc::unbounded_channel();
            inner.borrow_mut().ops = Some(ops);
            // One writer, so keystrokes and resizes reach the server in the
            // order they were typed.
            spawn_local(async move {
                while let Some(op) = ops_rx.recv().await {
                    let sent = match op {
                        Op::Data(bytes) => write.data(&bytes[..]).await,
                        Op::Resize(c, r) => write.window_change(c, r, 0, 0).await,
                        Op::Eof => write.eof().await,
                    };
                    if sent.is_err() {
                        break;
                    }
                }
            });
            spawn_local(async move {
                let mut status = JsValue::NULL;
                while let Some(msg) = read.wait().await {
                    match msg {
                        ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                            let _ = on_data.call1(&JsValue::NULL, &Uint8Array::from(&data[..]));
                        }
                        ChannelMsg::ExitStatus { exit_status } => status = exit_status.into(),
                        _ => {}
                    }
                }
                let _ = on_close.call1(&JsValue::NULL, &status);
            });
            Ok(JsValue::UNDEFINED)
        })
    }

    /// One SSH keepalive, driven by the page's own timer. Resolves `false`
    /// without sending when another call has the session, since the next
    /// beat will do.
    pub fn keepalive(&self) -> Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let Some(handle) = inner.borrow_mut().handle.take() else {
                return Ok(false.into());
            };
            let sent = handle.send_keepalive(false).await;
            inner.borrow_mut().handle = Some(handle);
            sent.map_err(rejected)?;
            Ok(true.into())
        })
    }

    /// Keystrokes, or anything else, to the shell.
    pub fn write(&self, data: &[u8]) {
        if let Some(ops) = &self.inner.borrow().ops {
            let _ = ops.send(Op::Data(data.to_vec()));
        }
    }

    /// The terminal changed size.
    pub fn resize(&self, cols: u32, rows: u32) {
        if let Some(ops) = &self.inner.borrow().ops {
            let _ = ops.send(Op::Resize(cols, rows));
        }
    }

    /// End the session: end of input, a disconnect, and the socket.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(ops) = inner.ops.take() {
            let _ = ops.send(Op::Eof);
        }
        if let Some(handle) = inner.handle.take() {
            spawn_local(async move {
                let _ = handle
                    .disconnect(Disconnect::ByApplication, "closed by the page", "en")
                    .await;
            });
        }
        let _ = inner.socket.ws.close();
    }
}

/// A key pair made in the page: the private half in OpenSSH form to keep,
/// the public half to put in `authorized_keys`.
#[wasm_bindgen(getter_with_clone)]
pub struct GeneratedKey {
    #[wasm_bindgen(js_name = privateOpenssh)]
    pub private_openssh: String,
    #[wasm_bindgen(js_name = publicOpenssh)]
    pub public_openssh: String,
    pub fingerprint: String,
}

/// A fresh Ed25519 key, from the browser's CSPRNG.
#[wasm_bindgen(js_name = generateKey)]
pub fn generate_key(comment: String) -> Result<GeneratedKey, JsValue> {
    let mut seed = [0u8; 32];
    getrandom02::getrandom(&mut seed).map_err(rejected)?;
    let mut key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    key.set_comment(comment);
    let public = key.public_key();
    Ok(GeneratedKey {
        private_openssh: key
            .to_openssh(LineEnding::LF)
            .map_err(rejected)?
            .to_string(),
        public_openssh: public.to_openssh().map_err(rejected)?,
        fingerprint: public.fingerprint(HashAlg::Sha256).to_string(),
    })
}
