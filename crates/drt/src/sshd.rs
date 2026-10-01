//! The REPL over SSH (SPEC.md §9): `drt start`'s `ssh` listener.
//!
//! A client that signs in with a key this deployment knows gets a PTY, and
//! the PTY gets a REPL instance of its own, the sealed one `drt repl` is,
//! holding the grants that key maps to. The SSH side is `drt-sshd`, the
//! same server a page runs; this file is the native host around it: the
//! socket, the keys and what each may do, and a terminal over the session.
//!
//! **Which keys, with which grants.** A key in `principals` gets that
//! principal's grants, which must sit inside what this deployment holds
//! (its `caps`, or everything when it names none); a grant outside is a
//! refusal at startup, by name. A key in the listener's `authorized_keys`
//! file, `~/.ssh/authorized_keys` when the listener names none, gets what
//! `drt repl` run here would hold: that file is the account's own list of
//! who may act as it, which is the stance sshd takes. A line in it carrying
//! options (`from=`, `command=`, `restrict`) is skipped with a warning
//! rather than honoured without its restriction. A key in both is the
//! principal.
//!
//! **One REPL per PTY**, on a thread of its own, so a line that computes
//! for a while holds up that session and no other. The REPL is
//! [`Repl::served`]: `print` comes back with its answers, since the core's
//! own `print` reaches this process's stdout and not the session, and
//! `:ssh` is refused, since it would take this process's terminal.
//!
//! Not here yet: the subsystem channel (SPEC.md §9's framed msgpack), and
//! attaching to a running deployment's instances rather than a fresh one.
//!
//! ## surface block
//!
//! - Entry points: [`spawn`], one `ssh` listener, bound before it returns;
//!   [`Keys::load`], who may sign in and with what.
//! - Configurable: none here; the REPL's own bounds (a line's printed
//!   output, a shown value) bound what one session can queue.
//! - Fan-out: [`Keys::load`]'s two sources, principals and an
//!   `authorized_keys` file; [`ShellTerminal`], the `ego_cli` terminal a
//!   session's REPL is edited through.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use drt_caps::{CapSet, Effect, Grant};
use drt_config::{Listener, RootConfig};
use drt_connector::Dispatcher;
use drt_sshd::{Authorized, HostKey, PublicKey, Shell, ShellReader, ShellWriter, Window};
use ego_cli::term::{Capabilities, Event, Size, Terminal};
use tokio::sync::mpsc;

use crate::repl::Repl;

/// Who may sign in, and what each key holds once it has.
#[derive(Clone)]
pub struct Keys {
    /// Key and grants, principals first so a key in both is the principal.
    entries: Vec<(PublicKey, Vec<Grant>)>,
}

impl Keys {
    /// The deployment's principals, then the listener's `authorized_keys`
    /// file (`~/.ssh/authorized_keys` when it names none and that exists).
    pub fn load(config: &RootConfig, listener: &Listener) -> Result<Keys, String> {
        Keys::from_sources(config, listener.authorized_keys.as_deref())
    }

    /// The same, with the key file named directly.
    pub fn from_sources(
        config: &RootConfig,
        authorized_keys: Option<&std::path::Path>,
    ) -> Result<Keys, String> {
        let ceiling = crate::config::ceiling(config);
        let held = CapSet::root(ceiling.clone());
        let denies: Vec<Grant> = ceiling
            .iter()
            .filter(|g| g.effect == Effect::Deny)
            .cloned()
            .collect();
        let mut entries = Vec::new();
        for (i, p) in config.principals.iter().enumerate() {
            let key =
                PublicKey::from_openssh(&p.key).map_err(|e| format!("principals[{i}].key: {e}"))?;
            for g in p.caps.iter().filter(|g| g.effect == Effect::Grant) {
                if !held.may_grant(&g.capability) {
                    return Err(format!(
                        "principals[{i}] is granted `{}`, which this deployment does \
                         not hold; a principal holds a part of what the deployment \
                         does, never more",
                        g.capability
                    ));
                }
            }
            let mut grants = p.caps.clone();
            grants.extend(denies.iter().cloned());
            entries.push((key, grants));
        }
        let file = match authorized_keys {
            Some(path) => Some(path.to_path_buf()),
            None => home()
                .map(|h| h.join(".ssh").join("authorized_keys"))
                .filter(|p| p.exists()),
        };
        if let Some(path) = file {
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match PublicKey::from_openssh(line) {
                    Ok(key) => {
                        if !entries.iter().any(|(k, _)| k.key_data() == key.key_data()) {
                            entries.push((key, ceiling.clone()));
                        }
                    }
                    // Most often a line with options in front of the key.
                    // Honouring the key without its restriction would grant
                    // more than the line does, so the line is skipped.
                    Err(_) => eprintln!(
                        "drt start: {} line {}: skipped; options such as from= or \
                         command= are not honoured here, so a restricted key is not \
                         admitted at all",
                        path.display(),
                        n + 1
                    ),
                }
            }
        }
        Ok(Keys { entries })
    }

    pub fn authorized(&self) -> Authorized {
        Authorized::from_keys(self.entries.iter().map(|(k, _)| k.clone()).collect())
    }

    pub fn grants(&self, key: &PublicKey) -> Option<Vec<Grant>> {
        self.entries
            .iter()
            .find(|(k, _)| k.key_data() == key.key_data())
            .map(|(_, g)| g.clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Bind one `ssh` listener and serve it on the process's runtime. Returns
/// once it is bound, with the address it bound.
pub fn spawn(config: &RootConfig, listener: &Listener) -> Result<SocketAddr, String> {
    let path = config.identity.host_key_path.as_ref().ok_or(
        "an `ssh` listener needs `identity.host_key_path`, the host key clients pin \
         (ssh-keygen -t ed25519 -N '' -f host_key makes one)",
    )?;
    let host_key = std::fs::read_to_string(path)
        .map_err(|e| format!("identity.host_key_path {}: {e}", path.display()))?;
    let fingerprint = HostKey::parse(&host_key)
        .map_err(|e| format!("identity.host_key_path {}: {e}", path.display()))?
        .fingerprint();
    let keys = Keys::load(config, listener)?;
    if keys.is_empty() {
        eprintln!(
            "drt start: the ssh listener on {} admits no key: name principals, or \
             an authorized_keys file",
            listener.address
        );
    }
    let socket = std::net::TcpListener::bind(&listener.address)
        .map_err(|e| format!("ssh listener {}: {e}", listener.address))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("ssh listener {}: {e}", listener.address))?;
    let addr = socket
        .local_addr()
        .map_err(|e| format!("ssh listener {}: {e}", listener.address))?;
    eprintln!(
        "drt start: ssh listening on {addr}, host key {fingerprint}, {} key(s) admitted",
        keys.len()
    );

    let config = Arc::new(config.clone());
    let _runtime = crate::runtime::enter();
    let handle = tokio::runtime::Handle::current();
    handle.spawn(async move {
        let Ok(socket) = tokio::net::TcpListener::from_std(socket) else {
            return;
        };
        while let Ok((stream, _peer)) = socket.accept().await {
            let _ = stream.set_nodelay(true);
            let Ok(key) = HostKey::parse(&host_key) else {
                return;
            };
            let (shells, mut opened) = mpsc::channel::<Shell>(4);
            let authorized = keys.authorized();
            tokio::spawn(async move {
                let _ = drt_sshd::serve(stream, key, authorized, shells).await;
            });
            let (keys, config) = (keys.clone(), config.clone());
            tokio::spawn(async move {
                while let Some(shell) = opened.recv().await {
                    match keys.grants(&shell.key) {
                        Some(grants) => session(shell, grants, config.clone()),
                        None => shell.close(1).await,
                    }
                }
            });
        }
    });
    Ok(addr)
}

// depth: one session

/// What goes to the client: bytes, in order, then how the session ended.
pub enum Out {
    Data(Vec<u8>),
    Exit(u32),
}

/// A REPL of its own for one shell, on a thread of its own.
pub fn session(shell: Shell, grants: Vec<Grant>, config: Arc<RootConfig>) {
    let window = shell.window.clone();
    let (reader, writer) = shell.split();
    let (out, rx) = mpsc::unbounded_channel::<Out>();
    tokio::spawn(forward(rx, writer));
    thread(ShellInput { reader, window }, out, grants, config);
}

/// A REPL of its own on one byte stream whose other end is a terminal:
/// `drt p2p`'s `repl` service (`doc/P2P.md` §5.2), where the stream carries
/// the PTY's bytes and `window` is what the peer reported over `control`.
/// Ends with the stream.
pub fn raw_session(
    io: std::pin::Pin<Box<dyn drt_rtc::host::Duplex>>,
    window: drt_rtc::Window,
    grants: Vec<Grant>,
    config: Arc<RootConfig>,
) {
    let (reader, mut writer) = tokio::io::split(io);
    let (out, mut rx) = mpsc::unbounded_channel::<Out>();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(next) = rx.recv().await {
            match next {
                Out::Data(bytes) => {
                    if writer.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
                Out::Exit(_) => {
                    let _ = writer.shutdown().await;
                    return;
                }
            }
        }
    });
    thread(RawInput { reader, window }, out, grants, config);
}

/// The thread one session runs on: the runtime's `print` goes to the
/// session, the REPL edits on its terminal, and the exit status follows.
fn thread<I: Input + 'static>(
    input: I,
    out: mpsc::UnboundedSender<Out>,
    grants: Vec<Grant>,
    config: Arc<RootConfig>,
) {
    std::thread::spawn(move || {
        let _runtime = crate::runtime::enter();
        let sink = out.clone();
        drt_platform::stdio::install_sink(Box::new(move |_fd, bytes| {
            let _ = sink.send(Out::Data(crlf(bytes)));
        }));
        let status = match run(input, out.clone(), grants, &config) {
            Ok(()) => 0,
            Err(e) => {
                let _ = out.send(Out::Data(crlf(format!("drt: {e}\n").as_bytes())));
                1
            }
        };
        drt_platform::stdio::uninstall_sink();
        let _ = out.send(Out::Exit(status));
    });
}

fn run<I: Input>(
    input: I,
    out: mpsc::UnboundedSender<Out>,
    grants: Vec<Grant>,
    config: &RootConfig,
) -> Result<(), String> {
    let dispatcher = Dispatcher::new(crate::cli::wire_connectors(config)?);
    let mut repl = Repl::served(Arc::new(dispatcher), grants, config.root.budget)?;
    let _ = out.send(Out::Data(crlf(format!("{}\n", repl.banner()).as_bytes())));
    let terminal = ShellTerminal::new(input, out);
    let mut editor = crate::repl::editor(&repl, terminal);
    futures_executor::block_on(crate::repl::edit(&mut repl, &mut editor))
}

async fn forward(mut rx: mpsc::UnboundedReceiver<Out>, writer: ShellWriter) {
    while let Some(next) = rx.recv().await {
        match next {
            Out::Data(bytes) => {
                if writer.write(bytes).await.is_err() {
                    return;
                }
            }
            Out::Exit(status) => {
                writer.close(status).await;
                return;
            }
        }
    }
}

/// Where a session's keystrokes and window come from: an SSH shell, or a
/// raw stream with the window reported beside it.
pub trait Input: Send {
    fn read(&mut self) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send;
    fn window(&self) -> (u32, u32);
}

struct ShellInput {
    reader: ShellReader,
    window: Window,
}

impl Input for ShellInput {
    async fn read(&mut self) -> Option<Vec<u8>> {
        self.reader.read().await
    }
    fn window(&self) -> (u32, u32) {
        self.window.get()
    }
}

struct RawInput {
    reader: tokio::io::ReadHalf<std::pin::Pin<Box<dyn drt_rtc::host::Duplex>>>,
    window: drt_rtc::Window,
}

impl Input for RawInput {
    async fn read(&mut self) -> Option<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 4096];
        match self.reader.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => {
                buf.truncate(n);
                Some(buf)
            }
        }
    }
    fn window(&self) -> (u32, u32) {
        self.window.get()
    }
}

/// The runtime's own output, as a terminal with no line discipline wants
/// it: every `\n` a `\r\n`. The editor's writes say `\r\n` themselves.
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

/// A session's shell as the terminal `ego_cli` edits a line on: the
/// client's keystrokes decoded as the page decodes xterm.js's, the
/// client's window as the size, and writes queued to the client.
pub struct ShellTerminal<I: Input> {
    input: I,
    out: mpsc::UnboundedSender<Out>,
    size: Size,
    decoder: ego_cli::decode::AnsiDecoder,
    pending: VecDeque<ego_cli::KeyPress>,
    /// The tail of a UTF-8 character split across two reads.
    partial: Vec<u8>,
}

impl<I: Input> ShellTerminal<I> {
    fn new(input: I, out: mpsc::UnboundedSender<Out>) -> Self {
        let (cols, rows) = input.window();
        ShellTerminal {
            input,
            out,
            size: Size::new(cols as u16, rows as u16),
            decoder: ego_cli::decode::AnsiDecoder::new(),
            pending: VecDeque::new(),
            partial: Vec::new(),
        }
    }
}

impl<I: Input> Terminal for ShellTerminal<I> {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            raw_mode: true,
            ansi: true,
            resize_events: true,
            line_discipline: false,
        }
    }

    fn size(&self) -> Size {
        self.size
    }

    fn set_raw(&mut self, _enabled: bool) -> ego_cli::Result<()> {
        // The client's pty is already raw: an SSH client puts its own
        // terminal in raw mode for the session.
        Ok(())
    }

    async fn next_event(&mut self) -> ego_cli::Result<Event> {
        loop {
            let (cols, rows) = self.input.window();
            let now = Size::new(cols as u16, rows as u16);
            if now != self.size {
                self.size = now;
                return Ok(Event::Resize(now));
            }
            if let Some(key) = self.pending.pop_front() {
                return Ok(Event::Key(key));
            }
            let Some(bytes) = self.input.read().await else {
                return Ok(Event::Eof);
            };
            self.partial.extend_from_slice(&bytes);
            let valid = match std::str::from_utf8(&self.partial) {
                Ok(_) => self.partial.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                // Not UTF-8 at all: drop it rather than wait forever.
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
        let _ = self.out.send(Out::Data(text.as_bytes().to_vec()));
        Ok(())
    }

    async fn flush(&mut self) -> ego_cli::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::crlf;

    #[test]
    fn a_bare_newline_returns_the_carriage_and_a_crlf_is_left_alone() {
        assert_eq!(crlf(b"a\nb\r\nc"), b"a\r\nb\r\nc");
        assert_eq!(crlf(b"\n\n"), b"\r\n\r\n");
    }
}
