//! An interactive SSH session: one shell on one host, on a terminal.
//!
//! Two callers, one implementation. `drt ssh` (and `:ssh` in the native
//! REPL) is a person at a terminal naming a host, so trust is OpenSSH's
//! `known_hosts` with a question on first use, and credentials are found
//! the way `ssh` finds them: the agent, then `~/.ssh` keys, then a
//! password. `host:ssh/shell` is a config naming the host, so trust is the
//! scope's pinned key, the credential is the scope's key, and nothing is
//! asked. [`Trust`] and [`Credentials`] are that difference, and it is
//! the only one.
//!
//! russh directly, not `ego_transport`'s client. That client is the
//! modern suite only (Ed25519 host and user keys) and decides the host key
//! inside its connect, after authentication has already started. A person
//! at a terminal needs RSA servers, password sign-in, and to be asked about
//! a key *before* anything authenticates, which is what a russh `Handler`
//! gives.
//!
//! ## surface block
//!
//! - Entry points: [`connect`] (key exchange, trust, sign-in, over any
//!   byte stream); [`shell`] (a pty and a shell, over a [`Terminal`]);
//!   [`on_this_terminal`] (both, on this process's tty); [`Login`],
//!   [`Trust`], [`Credentials`], [`Prompt`], [`TtyPrompt`].
//! - Configurable: [`TERM_DEFAULT`], [`IDENTITIES`], [`PASSWORD_TRIES`],
//!   [`RESIZE_POLL`], [`KEEPALIVE`].
//! - Fan-out: [`Trust`] (pinned, or `known_hosts` with or without asking);
//!   [`Credentials`] (one key, or discovery: agent, files, password); the
//!   channel messages [`shell`] acts on (data, stderr, exit status, eof,
//!   close, a refused request); [`Escape`], `~.` and `~~`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::ChannelMsg;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// The terminal type a pty is asked for when `$TERM` says nothing.
pub const TERM_DEFAULT: &str = "xterm-256color";
/// The keys discovery tries, under `~/.ssh`, in OpenSSH's order.
pub const IDENTITIES: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa"];
/// How many passwords a person may type before sign-in gives up.
pub const PASSWORD_TRIES: usize = 3;
/// How often the terminal's size is read for a resize.
pub const RESIZE_POLL: Duration = Duration::from_millis(250);
/// How often an idle session says it is alive, so a NAT or relay that
/// closes quiet connections does not close this one.
pub const KEEPALIVE: Duration = Duration::from_secs(30);

/// Who signs in where, trusting what.
pub struct Login {
    pub user: String,
    /// The name and port the host key is remembered under. With a tunnel
    /// in between, this is the name the person typed, not anything dialed.
    pub host: String,
    pub port: u16,
    pub trust: Trust,
    pub credentials: Credentials,
}

/// How the server's host key is judged.
pub enum Trust {
    /// Exactly these: `SHA256:…` fingerprints or OpenSSH public keys. A
    /// config's scope, or `--hostkey`.
    Pinned {
        fingerprints: Vec<String>,
        keys: Vec<PublicKey>,
    },
    /// OpenSSH's file. A changed key is always refused; an unknown one is
    /// asked about when `ask`, refused otherwise.
    KnownHosts { file: PathBuf, ask: bool },
}

/// How this side signs in.
pub enum Credentials {
    /// One key, and nothing else is tried.
    Key(Box<PrivateKey>),
    /// What `ssh` tries: the agent, then each file (asking for a
    /// passphrase when one is encrypted), then a password.
    Discover {
        agent: bool,
        files: Vec<PathBuf>,
        password: bool,
    },
}

/// The questions a sign-in may need answered. Asked before the session
/// takes the terminal, so a prompt reads a line the ordinary way.
pub trait Prompt: Send + Sync {
    fn confirm(&self, question: &str) -> bool;
    /// A secret, read without echo. `None` is "no answer": end of input.
    fn secret(&self, question: &str) -> Option<String>;
    fn say(&self, text: &str);
}

/// An authenticated connection, ready for a shell.
pub struct Connected {
    handle: Handle<Checker>,
    /// The server's host key, as `SHA256:…`.
    pub fingerprint: String,
}

/// What [`shell`] reads and writes. A tty is one; a test is another.
pub struct Terminal {
    /// Keystrokes, as bytes. Closing it sends end of file to the shell.
    pub input: mpsc::UnboundedReceiver<Vec<u8>>,
    pub output: Box<dyn Write + Send>,
    /// Columns and rows, read every [`RESIZE_POLL`].
    pub size: Box<dyn FnMut() -> (u32, u32) + Send>,
    pub term: String,
}

/// Default discovery: the agent, `~/.ssh`'s keys in OpenSSH's order, then
/// a password. `files` replaces the keys when it is not empty, as `-i`
/// does.
pub fn discover(files: Vec<PathBuf>) -> Credentials {
    let files = if files.is_empty() {
        home()
            .map(|h| IDENTITIES.iter().map(|n| h.join(".ssh").join(n)).collect())
            .unwrap_or_default()
    } else {
        files
    };
    Credentials::Discover {
        agent: true,
        files,
        password: true,
    }
}

/// `~/.ssh/known_hosts`, the file `ssh` reads and writes.
pub fn known_hosts() -> Option<PathBuf> {
    home().map(|h| h.join(".ssh").join("known_hosts"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// depth: the host key

struct Checker {
    trust: Arc<Trust>,
    host: String,
    port: u16,
    prompt: Arc<dyn Prompt>,
    /// The fingerprint offered, and why it was refused if it was.
    seen: Arc<Mutex<(Option<String>, Option<String>)>>,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            self.refuse(
                None,
                "the server offered a certificate, which this client does not take".into(),
            );
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).0 = Some(fingerprint.clone());
        match &*self.trust {
            Trust::Pinned { fingerprints, keys } => {
                let ok = fingerprints.contains(&fingerprint)
                    || keys.iter().any(|k| k.key_data() == key.key_data());
                if !ok {
                    self.refuse(
                        Some(&fingerprint),
                        "it is not the key this connection pins".into(),
                    );
                }
                Ok(ok)
            }
            Trust::KnownHosts { file, ask } => {
                match russh::keys::check_known_hosts_path(&self.host, self.port, key, file) {
                    Ok(true) => Ok(true),
                    Ok(false) if *ask => {
                        let question = format!(
                            "The host {} is not in {}.\nIts {} key is {fingerprint}.\nTrust it and continue?",
                            shown(&self.host, self.port),
                            file.display(),
                            key.algorithm(),
                        );
                        let prompt = self.prompt.clone();
                        let yes = tokio::task::spawn_blocking(move || prompt.confirm(&question))
                            .await
                            .unwrap_or(false);
                        if !yes {
                            self.refuse(Some(&fingerprint), "not trusted".into());
                            return Ok(false);
                        }
                        match russh::keys::known_hosts::learn_known_hosts_path(
                            &self.host, self.port, key, file,
                        ) {
                            Ok(()) => self.prompt.say(&format!(
                                "Added {} to {}.",
                                shown(&self.host, self.port),
                                file.display()
                            )),
                            Err(e) => self.prompt.say(&format!(
                                "Trusted for this session only: {} could not be written ({e}).",
                                file.display()
                            )),
                        }
                        Ok(true)
                    }
                    Ok(false) => {
                        self.refuse(
                            Some(&fingerprint),
                            format!("it is not in {}", file.display()),
                        );
                        Ok(false)
                    }
                    Err(russh::keys::Error::KeyChanged { line }) => {
                        self.refuse(
                            Some(&fingerprint),
                            format!(
                                "it is NOT the key {} line {line} records for this host. \
                                 Someone may be intercepting this connection, or the host's \
                                 key was replaced. Remove that line if you know why",
                                file.display()
                            ),
                        );
                        Ok(false)
                    }
                    Err(e) => {
                        self.refuse(
                            Some(&fingerprint),
                            format!("{} could not be read: {e}", file.display()),
                        );
                        Ok(false)
                    }
                }
            }
        }
    }
}

impl Checker {
    fn refuse(&self, _fingerprint: Option<&str>, why: String) {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(why);
    }
}

fn shown(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

// depth: connecting and signing in

/// Key exchange over `stream`, the host key judged by `login.trust`, and
/// sign-in by `login.credentials`. Nothing authenticates before the host
/// key has been accepted.
pub async fn connect<S>(
    stream: S,
    login: Login,
    prompt: Arc<dyn Prompt>,
) -> Result<Connected, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 4,
        nodelay: true,
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new((None, None)));
    let checker = Checker {
        trust: Arc::new(login.trust),
        host: login.host.clone(),
        port: login.port,
        prompt: prompt.clone(),
        seen: seen.clone(),
    };
    let target = shown(&login.host, login.port);
    let mut handle = match client::connect_stream(config, stream, checker).await {
        Ok(h) => h,
        Err(e) => {
            let (offered, why) = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
            return Err(match (offered, why) {
                (Some(fp), Some(why)) => format!("refused the host key of {target} ({fp}): {why}"),
                _ => format!("{target}: {e}"),
            });
        }
    };
    let fingerprint = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .clone()
        .unwrap_or_default();
    sign_in(
        &mut handle,
        &login.user,
        &target,
        login.credentials,
        &*prompt,
    )
    .await?;
    Ok(Connected {
        handle,
        fingerprint,
    })
}

async fn sign_in(
    handle: &mut Handle<Checker>,
    user: &str,
    target: &str,
    credentials: Credentials,
    prompt: &dyn Prompt,
) -> Result<(), String> {
    let refused = || format!("{user}@{target}: sign-in refused");
    match credentials {
        Credentials::Key(key) => {
            if with_key(handle, user, *key).await? {
                return Ok(());
            }
            Err(format!(
                "{}; the server did not accept the configured key",
                refused()
            ))
        }
        Credentials::Discover {
            agent,
            files,
            password,
        } => {
            if agent && with_agent(handle, user).await {
                return Ok(());
            }
            for file in files.iter().filter(|f| f.exists()) {
                let key = match russh::keys::load_secret_key(file, None) {
                    Ok(k) => k,
                    Err(russh::keys::Error::KeyIsEncrypted) => {
                        let Some(phrase) =
                            prompt.secret(&format!("Passphrase for {}: ", file.display()))
                        else {
                            continue;
                        };
                        match russh::keys::load_secret_key(file, Some(&phrase)) {
                            Ok(k) => k,
                            Err(e) => {
                                prompt.say(&format!("{}: {e}", file.display()));
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        prompt.say(&format!("{} skipped: {e}", file.display()));
                        continue;
                    }
                };
                if with_key(handle, user, key).await? {
                    return Ok(());
                }
            }
            if password {
                for _ in 0..PASSWORD_TRIES {
                    let Some(pw) = prompt.secret(&format!("{user}@{target}'s password: ")) else {
                        break;
                    };
                    let r = handle
                        .authenticate_password(user, pw)
                        .await
                        .map_err(|e| format!("{target}: {e}"))?;
                    if r.success() {
                        return Ok(());
                    }
                    prompt.say("Permission denied, please try again.");
                }
            }
            Err(format!(
                "{}: no key the server accepts{}",
                refused(),
                if password { ", and no password" } else { "" }
            ))
        }
    }
}

async fn with_key(
    handle: &mut Handle<Checker>,
    user: &str,
    key: PrivateKey,
) -> Result<bool, String> {
    let hash = rsa_hash(handle, key.algorithm().is_rsa()).await;
    let r = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
        .map_err(|e| e.to_string())?;
    Ok(r.success())
}

async fn rsa_hash(handle: &Handle<Checker>, rsa: bool) -> Option<HashAlg> {
    if !rsa {
        return None;
    }
    handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten()
}

#[cfg(unix)]
async fn with_agent(handle: &mut Handle<Checker>, user: &str) -> bool {
    use russh::keys::agent::client::AgentClient;
    use russh::keys::agent::AgentIdentity;
    let Ok(mut agent) = AgentClient::connect_env().await else {
        return false;
    };
    let Ok(ids) = agent.request_identities().await else {
        return false;
    };
    for id in ids {
        let AgentIdentity::PublicKey { key, .. } = id else {
            continue;
        };
        let hash = rsa_hash(handle, key.algorithm().is_rsa()).await;
        if let Ok(r) = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            if r.success() {
                return true;
            }
        }
    }
    false
}

#[cfg(not(unix))]
async fn with_agent(_handle: &mut Handle<Checker>, _user: &str) -> bool {
    false
}

// depth: the shell

/// `~` at the start of a line, then `.` to disconnect or `~` for one `~`:
/// OpenSSH's escape, because a session whose network died is otherwise
/// a terminal that ignores every key.
#[derive(Default)]
struct Escape {
    line_start: bool,
    pending: bool,
}

impl Escape {
    fn new() -> Self {
        Escape {
            line_start: true,
            pending: false,
        }
    }

    /// What to send, and whether to disconnect.
    fn filter(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if self.pending {
                self.pending = false;
                match b {
                    b'.' => return (out, true),
                    b'~' => out.push(b'~'),
                    _ => {
                        out.push(b'~');
                        out.push(b);
                    }
                }
            } else if self.line_start && b == b'~' {
                self.pending = true;
                continue;
            } else {
                out.push(b);
            }
            self.line_start = b == b'\r' || b == b'\n';
        }
        (out, false)
    }
}

/// A pty and a login shell on `conn`, until it ends. The exit status, when
/// the server reported one.
pub async fn shell(conn: Connected, mut term: Terminal) -> Result<Option<u32>, String> {
    let (cols, rows) = (term.size)();
    let mut channel = conn
        .handle
        .channel_open_session()
        .await
        .map_err(|e| format!("opening a session: {e}"))?;
    channel
        .request_pty(false, &term.term, cols, rows, 0, 0, &[])
        .await
        .map_err(|e| format!("asking for a terminal: {e}"))?;
    channel
        .request_shell(true)
        .await
        .map_err(|e| format!("asking for a shell: {e}"))?;

    let mut status = None;
    let mut size = (cols, rows);
    let mut escape = Escape::new();
    let mut input_open = true;
    let mut ticks = tokio::time::interval(RESIZE_POLL);
    let mut started = false;
    loop {
        tokio::select! {
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    started = true;
                    let _ = term.output.write_all(&data);
                    let _ = term.output.flush();
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status),
                Some(ChannelMsg::Success) => started = true,
                Some(ChannelMsg::Failure) if !started => {
                    let _ = channel.close().await;
                    return Err("the server refused a shell".into());
                }
                Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            bytes = term.input.recv(), if input_open => match bytes {
                Some(bytes) => {
                    let (send, quit) = escape.filter(&bytes);
                    if !send.is_empty() && channel.data(&send[..]).await.is_err() {
                        break;
                    }
                    if quit {
                        let _ = term.output.write_all(b"\r\nConnection closed (~.).\r\n");
                        let _ = channel.close().await;
                        break;
                    }
                }
                None => {
                    input_open = false;
                    let _ = channel.eof().await;
                }
            },
            _ = ticks.tick() => {
                let now = (term.size)();
                if now != size && now.0 > 0 && now.1 > 0 {
                    size = now;
                    let _ = channel.window_change(now.0, now.1, 0, 0).await;
                }
            }
        }
    }
    let _ = conn
        .handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    let _ = term.output.flush();
    Ok(status)
}

// depth: this process's terminal

/// Prompts on the controlling terminal, the ordinary way: a question on
/// stderr, a line from stdin, echo off for secrets.
pub struct TtyPrompt;

impl Prompt for TtyPrompt {
    fn confirm(&self, question: &str) -> bool {
        loop {
            eprint!("{question} (yes/no) ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                eprintln!();
                return false;
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "yes" | "y" => return true,
                "no" | "n" => return false,
                _ => eprintln!("Please type yes or no."),
            }
        }
    }

    fn secret(&self, question: &str) -> Option<String> {
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        let line = without_echo(|| {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line),
            }
        });
        eprintln!();
        line.map(|l| l.trim_end_matches(['\r', '\n']).to_string())
    }

    fn say(&self, text: &str) {
        eprintln!("{text}");
    }
}

#[cfg(unix)]
fn without_echo<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: tcgetattr/tcsetattr on fd 0 with a termios this function
    // owns; the saved settings are restored on every path out.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut saved) != 0 {
            return f();
        }
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        quiet.c_lflag |= libc::ECHONL;
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        let out = f();
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        out
    }
}

#[cfg(not(unix))]
fn without_echo<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// [`connect`] and [`shell`] on this process's terminal: prompts first,
/// then raw mode for the session, restored on every way out.
#[cfg(unix)]
pub async fn on_this_terminal<S>(stream: S, login: Login) -> Result<Option<u32>, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use std::io::IsTerminal;
    use std::sync::atomic::{AtomicBool, Ordering};

    if !std::io::stdin().is_terminal() {
        return Err("an interactive session needs a terminal on stdin".into());
    }
    let conn = connect(stream, login, Arc::new(TtyPrompt)).await?;

    let (tx, input) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || read_stdin(tx, &stop))
    };
    let raw = Raw::enter()?;
    let term = Terminal {
        input,
        output: Box::new(std::io::stdout()),
        size: Box::new(|| {
            // A terminal reporting no size (a pty opened without one) is
            // declining to answer, not one cell wide.
            match crossterm::terminal::size() {
                Ok((c, r)) if c > 0 && r > 0 => (u32::from(c), u32::from(r)),
                _ => (80, 24),
            }
        }),
        term: std::env::var("TERM")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| TERM_DEFAULT.to_string()),
    };
    let result = shell(conn, term).await;
    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    drop(raw);
    result
}

#[cfg(not(unix))]
pub async fn on_this_terminal<S>(_stream: S, _login: Login) -> Result<Option<u32>, String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Err("an interactive ssh session needs a Unix terminal in this build".into())
}

/// Raw mode for as long as this lives.
#[cfg(unix)]
struct Raw;

#[cfg(unix)]
impl Raw {
    fn enter() -> Result<Raw, String> {
        crossterm::terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
        Ok(Raw)
    }
}

#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// stdin to `tx` until `stop`. Polled with a timeout rather than blocked
/// in `read`, so the thread ends with the session: a reader left blocked
/// would take the first key typed at the REPL prompt afterwards.
#[cfg(unix)]
fn read_stdin(tx: mpsc::UnboundedSender<Vec<u8>>, stop: &std::sync::atomic::AtomicBool) {
    use std::sync::atomic::Ordering;
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd on the stack, for its length.
        let ready = unsafe { libc::poll(&mut fd, 1, 100) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            continue;
        }
        // SAFETY: reading into a buffer this function owns, at most its length.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        if tx.send(buf[..n as usize].to_vec()).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Escape;

    #[test]
    fn tilde_dot_at_a_line_start_disconnects() {
        let mut e = Escape::new();
        assert_eq!(e.filter(b"ls\r"), (b"ls\r".to_vec(), false));
        assert_eq!(e.filter(b"~."), (Vec::new(), true));
    }

    #[test]
    fn a_tilde_elsewhere_or_doubled_is_sent() {
        let mut e = Escape::new();
        assert_eq!(e.filter(b"a~."), (b"a~.".to_vec(), false));
        let mut e = Escape::new();
        assert_eq!(e.filter(b"~~"), (b"~".to_vec(), false));
        let mut e = Escape::new();
        assert_eq!(e.filter(b"~x"), (b"~x".to_vec(), false));
        // Split across reads.
        let mut e = Escape::new();
        assert_eq!(e.filter(b"~"), (Vec::new(), false));
        assert_eq!(e.filter(b"."), (Vec::new(), true));
    }
}
