//! An SSH server in a page (doc/SshInBrowser.md).
//!
//! Over [`crate::ws::WsStream`], so what reaches it is the page's socket
//! and what it hands back is a shell's two ends. The whole point is a
//! *standard* client: `ssh -o ProxyCommand="drt tunnel wss://..."` and a
//! terminal into a page, not a bespoke protocol between two things we
//! wrote.
//!
//! The posture is the ssh *client* connector's, pointed the other way, and
//! it is in the types rather than in a warning. There is no password
//! method and no "accept any key": [`Authorized`] is a list, an empty list
//! authenticates nobody, and a server that reaches [`serve`] without keys
//! is one that cannot be logged into. Getting this wrong by accident
//! should require typing a key in.
//!
//! ## surface block
//!
//! - [`Authorized`]: who may log in. `authorized_keys` lines, parsed once.
//! - [`HostKey`]: what the client pins. [`HostKey::generate`] makes one;
//!   the page is expected to *keep* it, because a host key that changes
//!   every load is a warning every load.
//! - [`serve`]: one connection. Returns when the client hangs up.
//! - [`Shell`]: one session channel, once the client asks for a shell --
//!   bytes both ways and the window size. This is a terminal in all but
//!   name, which is the point: `ego_cli`'s `Terminal` is a trait, M8
//!   implemented it over xterm.js, and this is the second thing that fits
//!   the same shape.
//! - [`Shell::split`]: the two halves separately, for a host that reads
//!   and writes from different places -- which a terminal's host does.
//! - [`Window`]: the client's size, live. Read it on every keystroke;
//!   that is how a resize reaches the thing drawing the line.
//! - [`Error`]: what a caller can be told.
//!
//! ## depth: what runs where
//!
//! `russh::server::run_stream` requires `H: Handler + Send`, and every
//! `Handler` method returns a `Send` future. So [`Handshake`] -- the
//! handler -- holds channel ends and nothing else, exactly as `WsStream`
//! does one layer down, and the JS side of a shell is somewhere else
//! entirely (`bindings.rs`, which is not `Send` and does not have to be).
//! That is the answer to the open question in doc/SshInBrowser.md §6: the
//! handler can be `Send`, by the same split that made the stream `Send`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use russh::keys::ssh_encoding::bytes::Bytes;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// How many chunks may queue from the client before the page is made to
/// catch up. The transport below has the same number for the same reason.
const DEPTH: usize = 256;

/// The window a client gets when it does not ask for one, or asks for
/// nothing: `ssh` run with a pipe for stdin requests a 0x0 pty, which is
/// legal on the wire and is not a terminal -- a line editor with no
/// columns renders a prompt and then has nowhere to put what is typed.
/// 80x24 is what `sshd` hands such a client, and what every terminal
/// defaults to when it has to guess.
const DEFAULT_WINDOW: (u32, u32) = (80, 24);

/// What a caller can be told went wrong. Deliberately coarse: a page
/// showing an SSH error to whoever is watching should not be narrating
/// which key failed.
#[derive(Debug)]
pub enum Error {
    /// A key or `authorized_keys` line that did not parse.
    Key(String),
    /// The connection ended badly, or never started.
    Session(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Key(e) => write!(f, "key: {e}"),
            Error::Session(e) => write!(f, "session: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The keys that may log in.
///
/// No variant means "anybody". An `Authorized` with nothing in it is the
/// honest representation of a page that has not decided yet, and it
/// authenticates nobody.
#[derive(Clone, Default)]
pub struct Authorized(Arc<Vec<PublicKey>>);

impl Authorized {
    /// Parse `authorized_keys` lines. Blank lines and `#` comments are
    /// skipped; anything else that does not parse is an error rather than
    /// a silently shorter list, because a typo that quietly removes a key
    /// is a lockout and a typo that quietly removes *all* of them would be
    /// worse.
    pub fn parse(lines: &str) -> Result<Self, Error> {
        let mut keys = Vec::new();
        for line in lines.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            keys.push(PublicKey::from_openssh(line).map_err(|e| Error::Key(e.to_string()))?);
        }
        Ok(Authorized(Arc::new(keys)))
    }

    /// Whether this key may log in. Compares the key itself, not a
    /// fingerprint of it and not the comment beside it.
    pub fn admits(&self, key: &PublicKey) -> bool {
        self.0.iter().any(|k| k.key_data() == key.key_data())
    }

    /// How many keys. A page showing "0" is showing a server nobody can
    /// reach, which is worth being able to say.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The key a client pins.
pub struct HostKey(PrivateKey);

impl HostKey {
    /// Read an OpenSSH private key.
    pub fn parse(openssh: &str) -> Result<Self, Error> {
        PrivateKey::from_openssh(openssh)
            .map(HostKey)
            .map_err(|e| Error::Key(e.to_string()))
    }

    /// A fresh Ed25519 key, as an OpenSSH private key the page should
    /// store. Returned as text rather than kept, because a host key this
    /// function does not hand back is a host key that changes on reload,
    /// and that trains whoever connects to click through the warning.
    pub fn generate() -> Result<String, Error> {
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519)
            .map_err(|e| Error::Key(e.to_string()))?;
        key.to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .map(|k| k.to_string())
            .map_err(|e| Error::Key(e.to_string()))
    }

    /// `SHA256:...`, the string `ssh` prints on first connection. A page
    /// that shows this beside the terminal is a page whose user can check
    /// the fingerprint instead of trusting on first use.
    pub fn fingerprint(&self) -> String {
        self.0.public_key().fingerprint(HashAlg::Sha256).to_string()
    }
}

/// The client's terminal size, live.
///
/// Live rather than a snapshot because a client resizing its window sends
/// a message mid-session, and the thing drawing a line has to know before
/// it wraps in the wrong place. Cheap to read on purpose: `ego_cli` asks a
/// terminal for its size on every keystroke, so this is one atomic load
/// and no lock.
#[derive(Clone)]
pub struct Window(Arc<AtomicU64>);

impl Window {
    fn new() -> Self {
        let it = Window(Arc::new(AtomicU64::new(0)));
        it.set(DEFAULT_WINDOW.0, DEFAULT_WINDOW.1);
        it
    }

    /// Both numbers in one atomic, so a reader can never see the columns
    /// of one size beside the rows of another.
    fn set(&self, cols: u32, rows: u32) {
        let cols = if cols == 0 { DEFAULT_WINDOW.0 } else { cols };
        let rows = if rows == 0 { DEFAULT_WINDOW.1 } else { rows };
        self.0
            .store((u64::from(cols) << 32) | u64::from(rows), Ordering::Relaxed);
    }

    /// Columns and rows, as of now.
    pub fn get(&self) -> (u32, u32) {
        let packed = self.0.load(Ordering::Relaxed);
        ((packed >> 32) as u32, packed as u32)
    }
}

/// One session channel, after the client asked for a shell.
///
/// Bytes in both directions and the window the client reported. What
/// consumes it is a terminal: M8's `ego_cli` stack in the page, or
/// anything else that reads and writes a terminal's bytes.
pub struct Shell {
    /// The client's window, in characters, and still changing.
    pub window: Window,
    from_client: mpsc::Receiver<Vec<u8>>,
    handle: russh::server::Handle,
    channel: ChannelId,
}

impl Shell {
    /// What the client typed, or `None` once the session is over.
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.from_client.recv().await
    }

    /// Write to the client's terminal. Fails once the session is gone.
    pub async fn write(&self, bytes: Vec<u8>) -> Result<(), Error> {
        self.writer().write(bytes).await
    }

    /// End the session with an exit status, the way a shell exiting does.
    pub async fn close(&self, status: u32) {
        self.writer().close(status).await
    }

    /// Reading and writing separately.
    ///
    /// A terminal's two directions are driven by different things -- one
    /// loop delivers keystrokes, another writes what the program said --
    /// and a host that must hold them apart should not have to hold the
    /// whole `Shell` in a lock to do it.
    pub fn split(self) -> (ShellReader, ShellWriter) {
        let writer = self.writer();
        (
            ShellReader {
                from_client: self.from_client,
            },
            writer,
        )
    }

    fn writer(&self) -> ShellWriter {
        ShellWriter {
            handle: self.handle.clone(),
            channel: self.channel,
        }
    }
}

/// What the client types.
pub struct ShellReader {
    from_client: mpsc::Receiver<Vec<u8>>,
}

impl ShellReader {
    /// What the client typed, or `None` once the session is over.
    pub async fn read(&mut self) -> Option<Vec<u8>> {
        self.from_client.recv().await
    }
}

/// What reaches the client's terminal. Cloneable, because the handle
/// underneath is: two writers are a program and its errors, not a race.
#[derive(Clone)]
pub struct ShellWriter {
    handle: russh::server::Handle,
    channel: ChannelId,
}

impl ShellWriter {
    /// Write to the client's terminal. Fails once the session is gone.
    pub async fn write(&self, bytes: Vec<u8>) -> Result<(), Error> {
        self.handle
            .data(self.channel, Bytes::from(bytes))
            .await
            .map_err(|_| Error::Session("the client is gone".into()))
    }

    /// End the session with an exit status, the way a shell exiting does.
    pub async fn close(&self, status: u32) {
        let _ = self.handle.exit_status_request(self.channel, status).await;
        let _ = self.handle.eof(self.channel).await;
        let _ = self.handle.close(self.channel).await;
    }
}

/// Serve one connection, and return when it ends.
///
/// `shells` receives a [`Shell`] each time a client asks for one. A
/// caller that drops the receiver ends up refusing shells, which is the
/// right thing rather than a leak: nothing is reading them.
pub async fn serve<S>(
    stream: S,
    host_key: HostKey,
    authorized: Authorized,
    shells: mpsc::Sender<Shell>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let config = Arc::new(Config {
        keys: vec![host_key.0],
        ..Default::default()
    });
    let handshake = Handshake {
        authorized,
        shells,
        to_page: None,
        window: Window::new(),
    };
    let running = russh::server::run_stream(config, stream, handshake)
        .await
        .map_err(|e| Error::Session(e.to_string()))?;
    running
        .await
        .map(|_| ())
        .map_err(|e| Error::Session(e.to_string()))
}

// ---------------------------------------------------------------------------
// depth: the handler, which holds channel ends so that it can be `Send`
// ---------------------------------------------------------------------------

struct Handshake {
    authorized: Authorized,
    shells: mpsc::Sender<Shell>,
    /// Set once a shell starts: where the client's keystrokes go.
    to_page: Option<mpsc::Sender<Vec<u8>>>,
    /// Shared with the shell this connection opens, so a resize after it
    /// started still lands. One per connection rather than per channel:
    /// this serves one shell, which is what a terminal session is.
    window: Window,
}

impl Handler for Handshake {
    type Error = russh::Error;

    /// Pubkey only, and only keys named in advance.
    ///
    /// Spelled out rather than inherited from the trait's default, because
    /// "there is no password method here" is the kind of fact that should
    /// survive somebody reading this file.
    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::reject())
    }

    /// Refuse an unknown key before a signature is verified. russh keeps
    /// the rejection constant-time either way; this only spares the work.
    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if self.authorized.admits(key) {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    /// The real check, after russh has proved the client holds the key.
    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if self.authorized.admits(key) {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    /// The window the client asked for. Zero means it could not say --
    /// see [`DEFAULT_WINDOW`].
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.window.set(cols, rows);
        session.channel_success(channel)?;
        Ok(())
    }

    /// The client resized. It lands in the same place the pty request
    /// did, so whatever is drawing reads the new size on its next
    /// keystroke without being told twice.
    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.window.set(cols, rows);
        session.channel_success(channel)?;
        Ok(())
    }

    /// A shell: the session's two ends go to whoever is listening, and
    /// the client is told it worked.
    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (to_page, from_client) = mpsc::channel(DEPTH);
        let shell = Shell {
            window: self.window.clone(),
            from_client,
            handle: session.handle(),
            channel,
        };
        if self.shells.send(shell).await.is_err() {
            // Nobody is taking shells. Refusing is the honest answer; the
            // alternative is a terminal that never echoes.
            session.channel_failure(channel)?;
            return Ok(());
        }
        self.to_page = Some(to_page);
        session.channel_success(channel)?;
        Ok(())
    }

    /// No `exec_request`: the trait's default fails the request, and a
    /// page has no command to run outside the shell it serves. `drt
    /// exec`'s posture is the native runtime's, and it does not follow the
    /// runtime into a browser by accident.
    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(to_page) = &self.to_page {
            let _ = to_page.send(data.to_vec()).await;
        }
        Ok(())
    }
}

/// The RNG `PrivateKey::random` wants -- russh's own choice, so this is
/// not a second opinion about entropy. In a page it seeds from
/// `crypto.getRandomValues`; natively, from the OS.
fn rng() -> rand::rngs::ThreadRng {
    rand::rng()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generated key round-trips, and its fingerprint is the string
    /// `ssh` shows.
    #[test]
    fn a_generated_host_key_is_openssh_and_has_a_fingerprint() {
        let openssh = HostKey::generate().unwrap();
        assert!(openssh.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        let key = HostKey::parse(&openssh).unwrap();
        assert!(key.fingerprint().starts_with("SHA256:"));
    }

    /// The rule the whole posture rests on: a key in the list gets in, a
    /// key that is not does not, and an empty list admits nobody.
    #[test]
    fn only_a_named_key_is_admitted() {
        let mine = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let theirs = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let line = mine.public_key().to_openssh().unwrap();

        let authorized = Authorized::parse(&line).unwrap();
        assert!(authorized.admits(mine.public_key()));
        assert!(!authorized.admits(theirs.public_key()));

        let nobody = Authorized::default();
        assert!(!nobody.admits(mine.public_key()));
        assert!(nobody.is_empty());
    }

    /// A client that cannot say how big its terminal is gets one anyway.
    /// `ssh` with a pipe for stdin asks for 0x0, and a line editor with no
    /// columns draws a prompt and then has nowhere to put a keystroke.
    #[test]
    fn a_window_of_nothing_is_the_default_window() {
        let window = Window::new();
        assert_eq!(window.get(), DEFAULT_WINDOW);

        window.set(0, 0);
        assert_eq!(window.get(), DEFAULT_WINDOW);
        window.set(120, 0);
        assert_eq!(window.get(), (120, DEFAULT_WINDOW.1));
        window.set(120, 40);
        assert_eq!(window.get(), (120, 40));
    }

    /// Comments and blank lines are skipped the way `authorized_keys`
    /// does; a line that is neither and does not parse is an error rather
    /// than a quietly shorter list.
    #[test]
    fn an_authorized_keys_file_parses_like_one() {
        let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
        let line = key.public_key().to_openssh().unwrap();
        let file = format!("# who may log in\n\n{line}\n");
        assert_eq!(Authorized::parse(&file).unwrap().len(), 1);

        let refused = Authorized::parse("ssh-ed25519 not-really-a-key");
        assert!(matches!(refused, Err(Error::Key(_))));
    }
}
