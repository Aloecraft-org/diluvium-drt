//! The `spawn` transport: DRT starts the plugin, the plugin dials back.
//!
//! # Surface
//!
//! Entry points:
//! - [`SpawnChannel::start`] — bind, mint a secret, start the plugin, wait
//!   for it to dial back and prove itself, and hold the stream.
//! - [`SpawnChannel::pid`] — the plugin's process, for a caller that
//!   names it in a message.
//! - [`SpawnChannel::port`] — where this host listened, for a test.
//!
//! Configurable values:
//! - [`SECRET_BYTES`] — how much entropy the shared secret carries.
//! - [`DIAL_BACK_TIMEOUT`] — how long a plugin has to come back.
//! - [`SECRET_ENV`] and [`DIAL_FLAG`] — how the plugin is told where to
//!   dial and what to say. These two names are the plugin-facing contract
//!   of this transport; changing either breaks every plugin written for it.
//!
//! Fan-out: none. One transport. `process` is the fork, `tcp` is the dial,
//! and this is the one that starts a process *and* gets a socket — which
//! is why `doc/Plugins.md` §4.1 calls it the native default.
//!
//! # Why this exists when `process` already works
//!
//! `process` hands the plugin its channel on fd 3, which is a unix idea to
//! the bone: there is no fd 3 on Windows, and `CreateProcess` inherits
//! handles by a different mechanism entirely. `tcp` works everywhere and
//! gives up the thing the fork gave — DRT does not start the plugin, does
//! not own its lifetime, and cannot know who is on the other end.
//!
//! `spawn` keeps both halves. DRT starts the process, so it owns the
//! lifetime and sweeps the tree at exit through
//! [`drt_platform::process::Tree`]; and it knows exactly who answered,
//! because the secret it minted a moment ago is the first thing that
//! arrives on the socket. Nothing is inherited, so the same code runs on
//! unix and on Windows.
//!
//! # The secret, and why it is not on the command line
//!
//! `doc/Plugins.md` §4.1 describes this transport as spawning the plugin
//! "with the port and a secret as arguments". The port is an argument
//! here. **The secret is not**, and the difference is deliberate.
//!
//! A loopback port has other ends: every process on the box can connect to
//! it, which is the whole reason a secret exists. But on Linux
//! `/proc/<pid>/cmdline` is world-readable, so a secret in `argv` is
//! legible to every other user on the machine for as long as the plugin
//! runs — the same mistake as a credential in a URL's query, which
//! `drt tunnel` moved to a header for exactly this reason. `/proc/<pid>/environ`
//! is readable only by the owner, and on Windows another user cannot read
//! a process's environment without debug privilege either. So the secret
//! travels in the environment, under [`SECRET_ENV`], and the doc's
//! sentence is the one thing here that was not followed to the letter.
//!
//! What the secret does and does not buy: it proves the process that
//! connected knows something only DRT and the program DRT started could
//! know, so a race by another local process is refused. It is not
//! authentication of *code* — a plugin is its own trust boundary, as
//! `manifest::Wiring` says at length — and it is not confidentiality,
//! because loopback traffic is readable by root regardless.

use std::io::{ErrorKind, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use drt_platform::process::Tree;

use crate::channel::{Channel, ChannelError};
use crate::tcp::TcpChannel;

/// How much entropy the shared secret carries. Thirty-two bytes, printed
/// as sixty-four hex characters: the size every other secret in this tree
/// is, and far past anything a local process could search in the seconds
/// the listener is open.
pub const SECRET_BYTES: usize = 32;

/// The environment variable the plugin reads its secret from.
///
/// Part of the plugin-facing contract; see the module header for why it is
/// the environment and not `argv`.
pub const SECRET_ENV: &str = "DRT_PLUGIN_SECRET";

/// The argument that tells the plugin where to dial, as `--drt-plugin-dial
/// 127.0.0.1:PORT`.
///
/// Spelled distinctly enough that it cannot collide with an argument the
/// manifest also passes, which is the only property the name needs.
pub const DIAL_FLAG: &str = "--drt-plugin-dial";

/// How long a plugin has to dial back before this host gives up on it.
///
/// Generous, because a plugin may be a script whose interpreter starts
/// cold, and this is paid once at startup rather than per call. A plugin
/// that exits before dialing is not waited on for this long: that is
/// noticed as soon as it happens.
pub const DIAL_BACK_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the accept loop looks, while waiting for the dial-back.
const POLL: Duration = Duration::from_millis(5);

/// A plugin this host started, and the socket it dialed back on.
#[derive(Debug)]
pub struct SpawnChannel {
    inner: TcpChannel,
    /// The plugin and everything it starts. Dropping sweeps, so a dropped
    /// channel is a dead plugin and not an orphan.
    _tree: Tree,
    pid: u32,
    port: u16,
}

impl SpawnChannel {
    /// Start `exec` and wait for it to dial back.
    ///
    /// Everything that can be refused is refused by name: a relative
    /// `exec`, a listener that will not bind, a plugin that will not
    /// start, one that exits before dialing back, one that never dials at
    /// all, and one that dials but cannot prove it is the program this
    /// host started.
    pub fn start(exec: &Path, argv: &[String]) -> Result<Self, ChannelError> {
        // `process`'s first discipline, and for its reason: what a
        // deployment wired is what runs, and a `PATH` lookup would make
        // that depend on the environment DRT happened to start in.
        if !exec.is_absolute() {
            return Err(ChannelError::Broken(format!(
                "a plugin's exec is an absolute path; '{}' is not, and this \
                 host does not search PATH for one",
                exec.display()
            )));
        }

        // Loopback only. A plugin listener reachable from the network
        // would be a hole the secret does not close, since the secret
        // proves who dialed and not who could.
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| ChannelError::Broken(format!("no loopback port for the plugin: {e}")))?;
        listener.set_nonblocking(true).map_err(|e| {
            ChannelError::Broken(format!("the plugin listener would not poll: {e}"))
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| ChannelError::Broken(format!("the plugin listener has no address: {e}")))?
            .port();

        let secret = mint_secret()?;

        let mut command = Command::new(exec);
        command
            .args(argv)
            .arg(DIAL_FLAG)
            .arg(format!("{}:{port}", Ipv4Addr::LOCALHOST))
            .env(SECRET_ENV, &secret)
            .stdin(Stdio::null())
            // Inherited on purpose, as `process` inherits them and for the
            // same reason: a plugin author's `print` has to go somewhere,
            // and it can safely go to stdout precisely because stdout is
            // not the channel here either.
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let (mut child, tree) = Tree::spawn(&mut command).map_err(|e| {
            ChannelError::Broken(format!(
                "the plugin '{}' would not start: {e}",
                exec.display()
            ))
        })?;
        let pid = child.id();

        // From here on every exit must take the plugin with it. `tree` is
        // moved into the returned channel on success and dropped -- which
        // sweeps -- on every error path below.
        let stream = accept_the_plugin(&listener, &secret, &mut child, exec)?;

        let inner = TcpChannel::from_stream(stream)?;
        Ok(Self {
            inner,
            _tree: tree,
            pid,
            port,
        })
    }

    /// The plugin's process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The loopback port this host listened on, which a test uses to be
    /// the plugin itself.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The far end of the accepted stream.
    pub fn peer(&self) -> SocketAddr {
        self.inner.peer()
    }
}

impl Channel for SpawnChannel {
    fn write_some(&mut self, bytes: &[u8]) -> Result<usize, ChannelError> {
        self.inner.write_some(bytes)
    }

    fn read_some(&mut self, out: &mut Vec<u8>) -> Result<usize, ChannelError> {
        self.inner.read_some(out)
    }
}

// depth: the dial-back, and the four ways it does not happen

/// Sixty-four hex characters from the platform CSPRNG.
fn mint_secret() -> Result<String, ChannelError> {
    let mut bytes = [0u8; SECRET_BYTES];
    drt_platform::entropy::fill(&mut bytes).map_err(|e| {
        ChannelError::Broken(format!(
            "a plugin needs a secret and this host has none: {e}"
        ))
    })?;
    let mut hex = String::with_capacity(SECRET_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Wait for a connection that proves it is the plugin this host started.
///
/// Another local process may connect first -- that is the case the secret
/// exists for -- so this does not take the first connection, it takes the
/// first one that answers correctly, and keeps waiting after one that does
/// not. A wrong answer is not fatal to the startup, only to that
/// connection, because treating it as fatal would hand any local process a
/// way to stop a plugin from ever starting.
fn accept_the_plugin(
    listener: &TcpListener,
    secret: &str,
    child: &mut std::process::Child,
    exec: &Path,
) -> Result<TcpStream, ChannelError> {
    let deadline = Instant::now() + DIAL_BACK_TIMEOUT;
    let mut refused = 0usize;
    loop {
        match listener.accept() {
            Ok((stream, from)) => match prove(&stream, secret) {
                Ok(true) => return Ok(stream),
                // Wrong secret, or a hangup partway through it. Dropped,
                // counted, and the wait goes on.
                Ok(false) | Err(_) => {
                    refused += 1;
                    let _ = from;
                    drop(stream);
                }
            },
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => {
                return Err(ChannelError::Broken(format!(
                    "the plugin listener failed: {e}"
                )))
            }
        }

        // A plugin that has already exited is never going to dial, and
        // saying so at once beats waiting out the timeout to say less.
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(ChannelError::Broken(format!(
                    "the plugin '{}' exited ({status}) before it dialed back",
                    exec.display()
                )))
            }
            Ok(None) => {}
            Err(e) => {
                return Err(ChannelError::Broken(format!(
                    "the plugin '{}' could not be waited on: {e}",
                    exec.display()
                )))
            }
        }

        if Instant::now() >= deadline {
            let refusals = match refused {
                0 => String::new(),
                1 => "; one connection did not know the secret".to_string(),
                n => format!("; {n} connections did not know the secret"),
            };
            return Err(ChannelError::Broken(format!(
                "the plugin '{}' did not dial back within {:?}{refusals}",
                exec.display(),
                DIAL_BACK_TIMEOUT
            )));
        }
        std::thread::sleep(POLL);
    }
}

/// Read exactly the secret's length and compare it in constant time.
///
/// Blocking, with a read timeout, and deliberately so: this happens once
/// per plugin at startup, before the channel is non-blocking and before a
/// single frame has moved, which is the same shape `tcp`'s blocking
/// `connect` at startup already has. A connection that sends nothing holds
/// this for the timeout and no longer.
fn prove(stream: &TcpStream, secret: &str) -> std::io::Result<bool> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(DIAL_BACK_TIMEOUT))?;
    let mut offered = vec![0u8; secret.len()];
    let mut read = stream;
    read.read_exact(&mut offered)?;
    Ok(constant_time_eq(&offered, secret.as_bytes()))
}

/// Equality that does not return early.
///
/// A timing attack across a loopback socket by a process that would have
/// to guess 256 bits is not a threat anyone need lose sleep over; writing
/// the comparison this way costs one line and removes the question.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A relative exec is refused before anything is bound or started,
    /// naming the path and saying that `PATH` is not consulted.
    #[test]
    fn a_relative_exec_is_refused_by_name() {
        let err = SpawnChannel::start(Path::new("plugin-echo"), &[]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("absolute path"), "{text}");
        assert!(text.contains("plugin-echo"), "{text}");
    }

    /// A program that exits at once is reported as having exited before
    /// dialing back, rather than as a timeout ten seconds later.
    #[test]
    fn a_plugin_that_exits_at_once_is_named_rather_than_waited_out() {
        let Some((exe, argv)) = quick_exit_program() else {
            return;
        };
        let started = Instant::now();
        let err = SpawnChannel::start(&exe, &argv).unwrap_err();
        let waited = started.elapsed();
        let text = err.to_string();
        assert!(
            text.contains("before it dialed back"),
            "the refusal did not name the early exit: {text}"
        );
        assert!(
            waited < DIAL_BACK_TIMEOUT,
            "an exited plugin was waited out for {waited:?}"
        );
    }

    /// The secret is hex, the right length, and different every time.
    #[test]
    fn the_secret_is_fresh_and_hex() {
        let a = mint_secret().unwrap();
        let b = mint_secret().unwrap();
        assert_eq!(a.len(), SECRET_BYTES * 2);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    /// Constant-time equality still decides equality correctly, which is
    /// the property a clever implementation most often loses.
    #[test]
    fn constant_time_equality_is_still_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    /// A program that exits at once, and the arguments that make it do
    /// so, spelled for the host.
    ///
    /// `start` appends its own dial argument after these, so whatever is
    /// chosen has to exit cleanly with an extra argument it did not ask
    /// for -- which both of these do.
    fn quick_exit_program() -> Option<(std::path::PathBuf, Vec<String>)> {
        #[cfg(unix)]
        {
            for candidate in ["/bin/true", "/usr/bin/true"] {
                let path = std::path::PathBuf::from(candidate);
                if path.exists() {
                    return Some((path, Vec::new()));
                }
            }
            None
        }
        #[cfg(windows)]
        {
            let path = std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe");
            path.exists()
                .then(|| (path, vec!["/c".to_string(), "exit".to_string()]))
        }
    }
}
